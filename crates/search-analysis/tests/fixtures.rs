//! Replays `_analyze` requests captured from Elasticsearch 9.5.5 (`tests/fixtures/*.json`,
//! written by `tests/fixtures/gen_fixtures.py`) and requires identical responses: the same tokens
//! with the same offsets, positions, types and explain attributes, or the same error type, reason
//! and status.
//!
//! `FIXTURES=<substring>` limits the run to matching case names; `FIXTURES_VERBOSE=1` prints every
//! mismatch in full; `FIXTURES_DUMP=<file>` writes the mismatching cases with our full output
//! (`{"name", "request", "actual"}` per line) for comparing against a live node.
//!
//! Cases over long texts (`long.json`) refer to texts stored once per file (`"text": {"$text":
//! key}` resolved from the file's `texts`) and record their token list as a digest (count and
//! FNV-1a hash, see [`digest`]) to keep the fixtures small.
//!
//! How the `thai` tokenizer divides Thai script depends on its word list. Elasticsearch uses the
//! JDK's, which is not ours to ship; set `ROLL_THAI_JDK_DICT=<path to the JDK's thai_dict>` to
//! install it and compare those cases exactly. Without it they are compared on everything the
//! dictionary does not decide: every token without Thai letters, with its offsets (cases stored
//! as digests, see `digest`, are skipped).

use std::path::Path;

use std::sync::Arc;

use search_analysis::{
    Analysis, AnalysisError, AnalyzeLimits, AnalyzeRequest, IndexOptions, ScriptHost, ScriptToken,
    TokenPredicate,
};
use serde_json::{Map, Value};

fn flatten(prefix: &str, v: &Value, out: &mut Map<String, Value>) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        _ => {
            out.insert(prefix.to_string(), v.clone());
        }
    }
}

/// A stand-in for Painless with the constant scripts the fixtures use.
struct ConstScripts;

struct Const(bool);

impl TokenPredicate for Const {
    fn test(&self, _t: &ScriptToken<'_>) -> Result<bool, AnalysisError> {
        Ok(self.0)
    }
}

impl ScriptHost for ConstScripts {
    fn compile_predicate(&self, script: &Value) -> Result<Arc<dyn TokenPredicate>, AnalysisError> {
        match script["source"].as_str() {
            Some("true") => Ok(Arc::new(Const(true))),
            Some("false") => Ok(Arc::new(Const(false))),
            other => Err(AnalysisError::illegal(format!(
                "unsupported test script {other:?}"
            ))),
        }
    }
}

/// Index settings as Elasticsearch normalizes them: the `analysis` subtree and the options.
fn index_settings(settings: &Value) -> (Value, IndexOptions) {
    let mut flat = Map::new();
    flatten("", settings, &mut flat);
    let mut analysis = Map::new();
    let mut opts = IndexOptions {
        scripts: Some(Arc::new(ConstScripts)),
        ..IndexOptions::default()
    };
    for (k, v) in flat {
        let k = k.strip_prefix("index.").unwrap_or(&k).to_string();
        if let Some(rest) = k.strip_prefix("analysis.") {
            analysis.insert(rest.to_string(), v);
        } else if k == "max_ngram_diff" {
            opts.max_ngram_diff = v.as_i64().unwrap_or(1) as i32;
        } else if k == "max_shingle_diff" {
            opts.max_shingle_diff = v.as_i64().unwrap_or(3) as i32;
        }
    }
    (Value::Object(analysis), opts)
}

fn run(case: &Value, pos: usize) -> Result<Value, AnalysisError> {
    let mut limits = AnalyzeLimits::default();
    if let Some(n) = case["index"]["analyze"]["max_token_count"].as_u64() {
        limits.max_token_count = n as usize;
    }
    if let Some(n) = case["index"]["analyze"]["max_char_count"].as_u64() {
        limits.max_char_count = n as usize;
    }
    let analysis = match case.get("index") {
        Some(settings) => {
            let (a, mut opts) = index_settings(settings);
            // The generator names the index after the case's position unless it records a name.
            opts.index_name = Some(
                case["index_name"]
                    .as_str()
                    .map_or_else(|| format!("fx-{pos}"), str::to_string),
            );
            Analysis::from_settings(&a, &opts)?
        }
        None => Analysis::global_with(IndexOptions {
            scripts: Some(Arc::new(ConstScripts)),
            ..IndexOptions::default()
        }),
    };
    if case.get("create").is_some() {
        return Ok(Value::String("index created".into()));
    }
    let req = AnalyzeRequest::from_json(&case["request"])?;
    let field_analyzer = req.field.as_ref().and_then(|f| {
        let props = &case["index"]["_mappings"]["properties"];
        if let Some(name) = props[f]["analyzer"].as_str() {
            return analysis.analyzer(name);
        }
        // `search_as_you_type` subfields: the field's analyzer wrapped with shingles (and
        // edge n-grams for `._index_prefix`).
        let (parent, sub) = f.split_once('.')?;
        let p = &props[parent];
        if p["type"] != "search_as_you_type" {
            return None;
        }
        let base = match p["analyzer"].as_str() {
            Some(name) => analysis.analyzer(name)?,
            None => analysis.default_analyzer(),
        };
        let max = p["max_shingle_size"].as_u64().unwrap_or(3) as usize;
        match sub {
            "_index_prefix" => base.with_shingles_and_prefixes(max).ok(),
            gram => {
                let n = gram.strip_prefix('_')?.strip_suffix("gram")?.parse().ok()?;
                base.with_shingles(n).ok()
            }
        }
    });
    analysis.analyze(&req, field_analyzer.as_ref(), limits)
}

fn error_json(e: &AnalysisError) -> Value {
    serde_json::json!({"type": e.kind, "reason": e.reason, "caused_by": e.caused_by.as_ref().map(|c| serde_json::json!({"type": c.kind, "reason": c.reason}))})
}

/// Whether a case's output depends on the Thai dictionary.
fn thai_dictionary_dependent(case: &Value) -> bool {
    let req = &case["request"];
    let thai = req["tokenizer"] == "thai" || req["analyzer"] == "thai";
    let text = match &req["text"] {
        Value::String(s) => s.clone(),
        Value::Array(v) => v.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    };
    thai && text.chars().any(|c| ('\u{0E01}'..='\u{0E4E}').contains(&c))
}

/// The dictionary-independent view of a token list: the tokens without Thai letters. (Which
/// parts of a Thai run become tokens depends on the dictionary too: a piece starting with a
/// vowel sign is dropped, a stopword is removed.)
fn dictionary_independent(v: &Value) -> Vec<Value> {
    let mut other = Vec::new();
    for t in v["tokens"].as_array().into_iter().flatten() {
        let term = t["token"].as_str().unwrap_or("");
        if !term.chars().any(|c| ('\u{0E01}'..='\u{0E4E}').contains(&c)) {
            let mut t = t.clone();
            if let Some(m) = t.as_object_mut() {
                m.remove("position");
            }
            other.push(t);
        }
    }
    other
}

/// The generator's digest of a long token list (`digest` in `gen_fixtures.py`): the count, a
/// 64-bit FNV-1a hash of one line per token, and the first and last tokens.
fn digest(tokens: &[Value]) -> Value {
    let mut h: u64 = 0xcbf29ce484222325;
    let short = |t: &Value| {
        let term: String = t["token"].as_str().unwrap_or("").chars().take(40).collect();
        serde_json::json!([
            term,
            t["start_offset"],
            t["end_offset"],
            t["type"],
            t["position"]
        ])
    };
    for t in tokens {
        let pl = t
            .get("positionLength")
            .map(|v| v.to_string())
            .unwrap_or_default();
        let line = format!(
            "{}\t{}\t{}\t{}\t{}\t{pl}\n",
            t["token"].as_str().unwrap_or(""),
            t["start_offset"],
            t["end_offset"],
            t["type"].as_str().unwrap_or(""),
            t["position"]
        );
        for b in line.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    serde_json::json!({
        "count": tokens.len(),
        "fnv": format!("{h:016x}"),
        "head": tokens.iter().take(8).map(short).collect::<Vec<_>>(),
        "tail": tokens[tokens.len().saturating_sub(3)..].iter().map(short).collect::<Vec<_>>(),
    })
}

/// An error's reason without the `[line:column]` of the offending token that parse errors
/// carry: the REST layer, which has the raw body, prepends it.
fn without_location(err: &Value) -> Value {
    let reason = &err["reason"];
    // Jackson's "Expected text at <line>:<column> but found …".
    if let Some(r) = reason.as_str()
        && let Some(rest) = r.strip_prefix("Expected text at ")
        && let Some((_, tail)) = rest.split_once(" but found ")
    {
        return Value::String(format!("Expected text but found {tail}"));
    }
    if err["type"] == "x_content_parse_exception"
        && let Some(r) = reason.as_str()
        && let Some((loc, rest)) = r.strip_prefix('[').and_then(|r| r.split_once("] "))
        && loc
            .split(':')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        return Value::String(rest.to_string());
    }
    reason.clone()
}

/// Compares an actual result with the recorded response; None when they agree.
fn compare(
    case: &Value,
    actual: &Result<Value, AnalysisError>,
    exact_thai: bool,
) -> Option<String> {
    if case["compact"] == true
        && case["status"] == 200
        && let Ok(v) = actual
    {
        // A digest cannot be narrowed to the dictionary-independent tokens.
        if !exact_thai && thai_dictionary_dependent(case) {
            return None;
        }
        let empty = Vec::new();
        let d = digest(v["tokens"].as_array().unwrap_or(&empty));
        return (d != case["response"])
            .then(|| format!("expected digest {}\n   actual digest {d}", case["response"]));
    }
    // A `limit` after the thai tokenizer cuts where the dictionary's word count says, so
    // everything after the cut depends on the dictionary too.
    let limited = case["request"]["filter"]
        .as_array()
        .is_some_and(|f| f.iter().any(|f| f["type"] == "limit"));
    if !exact_thai && limited && thai_dictionary_dependent(case) {
        return None;
    }
    if !exact_thai
        && thai_dictionary_dependent(case)
        && case["status"] == 200
        && let Ok(v) = actual
    {
        let (e, a) = (
            dictionary_independent(&case["response"]),
            dictionary_independent(v),
        );
        return (e != a).then(|| format!("expected {:?}\n   actual {:?}", e, a));
    }
    let status = case["status"].as_u64().unwrap_or(0);
    let expected = &case["response"];
    match actual {
        Ok(v) if status == 200 => {
            (v != expected).then(|| format!("expected {expected}\n   actual {v}"))
        }
        Ok(v) => Some(format!(
            "expected error {status} {}\n   actual {v}",
            expected["error"]
        )),
        Err(e) if status == 200 => Some(format!(
            "expected {expected}\n   actual error {}",
            error_json(e)
        )),
        Err(e) => {
            let err = &expected["error"];
            let mut diffs = Vec::new();
            if err["type"] != e.kind {
                diffs.push(format!("type {} != {}", err["type"], e.kind));
            }
            let reason = without_location(err);
            if reason != e.to_json()["reason"] {
                diffs.push(format!("reason {} != {:?}", err["reason"], e.reason));
            }
            if status != e.status as u64 {
                diffs.push(format!("status {status} != {}", e.status));
            }
            if let Some(rc) = err["root_cause"].get(0) {
                let r = e.root_cause();
                if rc["type"] != r.kind || without_location(rc) != r.to_json()["reason"] {
                    diffs.push(format!("root_cause {rc} != {} {:?}", r.kind, r.reason));
                }
            }
            // The whole cause chain, as far as Elasticsearch reports one.
            let mut expected_cause = err.get("caused_by");
            let mut actual_cause = e.caused_by.as_deref();
            while let Some(cb) = expected_cause {
                match actual_cause {
                    Some(c)
                        if cb["type"] == c.kind
                            && without_location(cb) == c.to_json()["reason"] => {}
                    Some(c) => {
                        diffs.push(format!("caused_by {cb} != {} {:?}", c.kind, c.reason));
                        break;
                    }
                    None => {
                        diffs.push(format!("caused_by {cb} missing"));
                        break;
                    }
                }
                expected_cause = cb.get("caused_by");
                actual_cause = actual_cause.and_then(|c| c.caused_by.as_deref());
            }
            (!diffs.is_empty()).then(|| diffs.join("; "))
        }
    }
}

#[test]
fn elasticsearch_fixtures() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let known: Vec<String> = std::fs::read_to_string(dir.join("known_divergences.txt"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
        .collect();
    let exact_thai = match std::env::var_os("ROLL_THAI_JDK_DICT") {
        Some(path) => {
            let data = std::fs::read(&path).expect("ROLL_THAI_JDK_DICT");
            let dict = search_analysis::ThaiDictionary::from_jdk(&data).expect("JDK thai_dict");
            search_analysis::set_thai_dictionary(dict).expect("dictionary already in use");
            true
        }
        None => false,
    };
    let filter = std::env::var("FIXTURES").ok();
    let verbose = std::env::var_os("FIXTURES_VERBOSE").is_some();
    let mut dump =
        std::env::var_os("FIXTURES_DUMP").map(|p| std::fs::File::create(p).expect("FIXTURES_DUMP"));
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    files.sort();
    let (mut total, mut failed, mut diverged) = (0, 0, 0);
    let mut report = String::new();
    for file in files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
    {
        let group = file.file_stem().unwrap().to_string_lossy().to_string();
        let data: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let texts = &data["texts"];
        let (mut g_total, mut g_failed) = (0, 0);
        for (pos, case) in data["cases"].as_array().unwrap().iter().enumerate() {
            let name = format!("{group}/{}", case["name"].as_str().unwrap_or("?"));
            if let Some(f) = &filter
                && !name.contains(f.as_str())
            {
                continue;
            }
            g_total += 1;
            // Long texts are stored once per file and referenced as {"$text": key}.
            let resolved;
            let case = match case["request"]["text"]["$text"].as_str() {
                Some(key) => {
                    let mut c = case.clone();
                    c["request"]["text"] = texts[key].clone();
                    resolved = c;
                    &resolved
                }
                None => case,
            };
            let actual = std::panic::catch_unwind(|| run(case, pos))
                .unwrap_or_else(|_| Err(AnalysisError::illegal("PANIC")));
            let is_known = known.contains(&name);
            let diff = compare(case, &actual, exact_thai);
            if is_known {
                match diff {
                    Some(_) => diverged += 1,
                    None => eprintln!(
                        "known divergence {name} now matches; remove it from known_divergences.txt"
                    ),
                }
                continue;
            }
            if let Some(diff) = diff {
                g_failed += 1;
                if let Some(f) = dump.as_mut() {
                    use std::io::Write;
                    let actual = match &actual {
                        Ok(v) => v.clone(),
                        Err(e) => serde_json::json!({"error": e.to_json()}),
                    };
                    let line = serde_json::json!({"name": name, "request": case["request"], "index": case.get("index"), "actual": actual});
                    writeln!(f, "{line}").expect("FIXTURES_DUMP");
                }
                let line = if verbose {
                    diff
                } else {
                    diff.chars().take(600).collect()
                };
                let request: String = case["request"].to_string().chars().take(400).collect();
                report.push_str(&format!(
                    "MISMATCH {name}\n   request {request}\n   {line}\n"
                ));
            }
        }
        total += g_total;
        failed += g_failed;
        eprintln!("{group}: {}/{g_total} match", g_total - g_failed);
    }
    if failed > 0 {
        eprintln!("{report}");
    }
    eprintln!(
        "total: {}/{total} match ({diverged} known divergences)",
        total - failed
    );
    assert_eq!(
        failed, 0,
        "{failed} of {total} fixture cases differ from Elasticsearch"
    );
}
