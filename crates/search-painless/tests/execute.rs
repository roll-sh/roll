//! Runs every captured `_execute` fixture (`tests/fixtures/*.json`, produced by
//! `tests/fixtures/capture.py` against Elasticsearch 9.5.5) through the engine and compares the
//! result string or the whole error body with what Elasticsearch answered.

use std::path::Path;

use search_painless::{Context, Engine, NoHost, Settings, Value};
use serde_json::{json, Value as Json};

#[path = "support/mock_host.rs"]
mod mock_host;

/// Identity hash codes (`[I@62608e13`) differ between runs; such results only compare their
/// prefix.
fn has_identity_hash(s: &str) -> bool {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'@' && i + 1 < b.len() && b[i + 1].is_ascii_hexdigit() {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j - i > 5 {
                return true;
            }
        }
    }
    false
}

fn strip_hashes(s: &str) -> String {
    let mut out = String::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        out.push(b[i]);
        if b[i] == '@' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j - i > 5 {
                out.push_str("HASH");
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

pub fn run_one(engine: &Engine, case: &Json) -> Result<(), String> {
    let req = &case["request"];
    let script = &req["script"];
    let src = script["source"].as_str().unwrap();
    let params = script.get("params").cloned().unwrap_or(json!({}));
    let ctx_name = req.get("context").and_then(|c| c.as_str()).unwrap_or("painless_test");
    let Some(ctx) = Context::from_name(ctx_name) else { return Err(format!("unknown context {ctx_name}")) };
    if std::env::var("PAINLESS_TRACE").is_ok() {
        eprintln!("CASE {src}");
    }
    let status = case["status"].as_u64().unwrap();
    let expected = &case["response"];
    let got: (u64, Json) = match engine.compile(src, ctx) {
        Err(e) => (e.status as u64, e.to_response()),
        Ok(s) => {
            let mut inst = s.instance();
            inst.set_params(&params);
            if ctx == Context::PainlessTest {
                match inst.execute(&mut NoHost) {
                    Ok(v) => match v.try_to_java_string() {
                        Ok(s) => (200, json!({ "result": s })),
                        // the response fails to serialize: a plain 500 error, no script_exception
                        Err(e) => {
                            let reason = if e.reason.is_empty() { Json::Null } else { json!(e.reason) };
                            (500, json!({ "error": { "root_cause": [{ "type": e.kind, "reason": reason }], "type": e.kind, "reason": reason }, "status": 500 }))
                        }
                    },
                    Err(e) => (e.status as u64, e.to_response()),
                }
            } else {
                let setup = &req["context_setup"];
                let index = setup["index"].as_str().unwrap_or("").to_string();
                let mapping: Json = serde_json::from_str(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{index}.mapping.json"))).unwrap()).unwrap();
                let mut host = mock_host::MockHost { index, mapping, doc: setup["document"].clone(), score: 0.0, seq_no: 0, search: false };
                match inst.execute(&mut host) {
                    Ok(v) => (200, json!({ "result": render(ctx, &v, &inst) })),
                    Err(e) => (e.status as u64, e.to_response()),
                }
            }
        }
    };
    let (gs, gj) = got;
    if status == 200 && ctx != Context::PainlessTest {
        let (mut want, mut have) = (expected["result"].clone(), gj["result"].clone());
        normalize(&mut want);
        normalize(&mut have);
        if gs == 200 && num_eq(&want, &have) {
            return Ok(());
        }
        return Err(format!("{src}\n  want: {}\n  got:  {gs} {}", expected["result"], if gs == 200 { gj["result"].to_string() } else { gj.to_string() }));
    }
    if status == 200 {
        let want = expected["result"].as_str().unwrap_or("");
        let have = gj.get("result").and_then(|r| r.as_str()).unwrap_or("");
        if gs == 200 && (want == have || (has_identity_hash(want) && strip_hashes(want) == strip_hashes(have))) {
            return Ok(());
        }
        return Err(format!("{src}\n  want: {want}\n  got:  {gs} {}", if gs == 200 { have.to_string() } else { gj.to_string() }));
    }
    let mut want = expected.clone();
    let mut have = gj.clone();
    normalize(&mut want);
    normalize(&mut have);
    if gs == status && want == have {
        return Ok(());
    }
    Err(format!("{src}\n  want: {status} {want}\n  got:  {gs} {have}"))
}

/// JSON equality with numbers compared as doubles (Elasticsearch writes `1.0`, we may write `1`).
fn num_eq(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| num_eq(p, q)),
        (Json::Object(x), Json::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| num_eq(v, w))),
        _ => a == b,
    }
}

fn fmt_millis(ms: i64) -> String {
    let d = chrono::DateTime::from_timestamp_millis(ms).unwrap();
    d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// What the `_execute` API answers for a document context's result.
fn render(ctx: Context, v: &Value, inst: &search_painless::Instance) -> Json {
    let emitted: Vec<Json> = inst.emitted().iter().map(|x| x.to_json()).collect();
    match ctx {
        Context::Score => Value::Double(v.as_f64().unwrap_or(0.0)).to_json(),
        Context::Filter => v.to_json(),
        Context::BooleanField => {
            let mut b: Vec<bool> = emitted.iter().map(|x| x.as_bool().unwrap_or(false)).collect();
            b.sort();
            json!(b)
        }
        Context::DateField => Json::Array(emitted.iter().map(|x| json!(fmt_millis(x.as_i64().unwrap_or(0)))).collect()),
        Context::GeoPointField => Json::Array(emitted.iter().map(|x| json!({"coordinates": [x["lon"], x["lat"]], "type": "Point"})).collect()),
        Context::CompositeField => {
            let mut o = serde_json::Map::new();
            for (k, vals) in inst.emitted_fields() {
                o.insert(k.clone(), Json::Array(vals.iter().map(|x| x.to_json()).collect()));
            }
            Json::Object(o)
        }
        _ => Json::Array(emitted),
    }
}

fn normalize(v: &mut Json) {
    // java.util.regex internals between the char reader and the Matcher entry point depend on
    // how the JDK compiled the pattern
    if let Json::Object(o) = v
        && let Some(Json::Array(st)) = o.get_mut("script_stack") {
            st.retain(|f| !f.as_str().is_some_and(|s| s.starts_with("java.base/java.util.regex.Pattern$")));
        }
    match v {
        Json::String(s) => {
            if has_identity_hash(s) {
                *s = strip_hashes(s);
            }
        }
        Json::Array(a) => a.iter_mut().for_each(normalize),
        Json::Object(o) => o.values_mut().for_each(normalize),
        _ => {}
    }
}

/// Runs a fixture file on a thread with a large stack: unoptimized builds use far more stack
/// per level than release builds, and cases that nest deeply (`limits.json`) must get as far
/// as Elasticsearch does.
fn run_file(name: &str) -> (usize, Vec<(String, String)>) {
    let name = name.to_string();
    std::thread::Builder::new().stack_size(256 << 20).spawn(move || run_file_here(&name)).unwrap().join().unwrap()
}

fn run_file_here(name: &str) -> (usize, Vec<(String, String)>) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let cases: Vec<Json> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let engine = Engine::new(Settings { max_compilations_rate: None, stack_bytes: 128 << 20, ..Settings::default() });
    let mut failures = Vec::new();
    for c in &cases {
        let src = c["request"]["script"]["source"].as_str().unwrap_or("").to_string();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_one(&engine, c)));
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => failures.push((src, e)),
            Err(p) => {
                let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                failures.push((src.clone(), format!("{src}\n  PANIC: {msg}")))
            }
        }
    }
    (cases.len(), failures)
}

/// Sources of cases known to differ from Elasticsearch (`tests/fixtures/known_diffs.json`:
/// `{"<fixture file>": [{"source": …, "why": …}]}`). They are reported but do not fail the run;
/// a known case that starts passing fails it so the list stays accurate.
fn known_diffs(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/known_diffs.json");
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let all: Json = serde_json::from_str(&text).unwrap();
    all.get(name)
        .and_then(|l| l.as_array())
        .map(|l| l.iter().filter_map(|c| c["source"].as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Runs fixture files and fails if any case differs from Elasticsearch, after running them all.
fn check(names: &[&str]) {
    let mut bad = Vec::new();
    for name in names {
        let (n, failures) = run_file(name);
        let known = known_diffs(name);
        let mut unexpected = 0;
        for (src, f) in &failures {
            if known.contains(src) {
                if std::env::var("PAINLESS_KNOWN").is_ok() {
                    eprintln!("---- (known) {f}");
                }
            } else {
                unexpected += 1;
                eprintln!("---- {f}");
            }
        }
        let fixed: Vec<&String> = known.iter().filter(|k| !failures.iter().any(|(s, _)| s == *k)).collect();
        for k in &fixed {
            eprintln!("---- now passing, remove from known_diffs.json: {k}");
        }
        if unexpected > 0 || !fixed.is_empty() {
            bad.push(format!("{name}: {unexpected} of {n} cases differ from Elasticsearch ({} known)", failures.len() - unexpected));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The language: casts, operators, promotions, boxing, `def` dispatch, lambdas and references,
/// statements, regexes, syntax and compile errors, everyday scripts, `subList` views (`lang*.json`
/// but the JDK library's, the scripts of Elasticsearch's own Painless language tests in
/// `estests.json`, `views.json`), and the review's captures (`review1.json`: `BigInteger` range
/// overflow, `HexFormat`, `parseUnsigned*`/`decode`, case-insensitive compares, the JDK 27 zone
/// IDs, `StrictMath` bit-exact with fdlibm).
#[test]
fn language() {
    check(&["lang.json", "lang2.json", "lang3.json", "lang4.json", "lang5.json", "lang8.json", "estests.json", "views.json", "review1.json"]);
}

/// The JDK library: collections, streams and the JDK frames of their failures (`lang6.json`,
/// `lang7.json`, `lang9.json`), `java.lang.Math` (`math.json`), `java.nio` buffers (`nio.json`).
#[test]
fn java_library() {
    check(&["lang6.json", "lang7.json", "lang9.json", "math.json", "nio.json"]);
}

/// `java.time` (week fields, locale-dependent text, zone names and rules), `TimeZone`,
/// `Calendar` and the `java.text` date formats, `java.util.Locale` (`time*.json`,
/// `legacy_dates.json`, `locale.json`).
#[test]
fn time_and_locales() {
    check(&["time.json", "time2.json", "time3.json", "legacy_dates.json", "locale.json"]);
}

/// Strings and text: `String.format`, characters, `java.text` number formats and
/// normalization, `MessageFormat`/`ChoiceFormat` (`format.json`, `chars.json`, `text.json`,
/// `message.json`).
#[test]
fn text_and_formats() {
    check(&["format.json", "chars.json", "text.json", "message.json"]);
}

/// Document contexts: `params` typing, score, filter and runtime-field scripts, the field API,
/// doc-value reads, loops and conversions, vector fields of every element type (`docs*.json`,
/// `docloops.json`, `vectors2.json`).
#[test]
fn documents() {
    check(&["docs.json", "docs2.json", "docs3.json", "docloops.json", "vectors2.json"]);
}

#[test]
fn moving_functions() {
    check(&["movfn.json"]);
}

#[test]
fn grok_and_dissect() {
    check(&["extract.json"]);
}

/// Deep and large values: nesting limits of `Json.dump`, deep `toString`/`hashCode`/`equals`,
/// dropping deeply nested values, array sizes (`limits.json`).
#[test]
fn limits() {
    check(&["limits.json"]);
}

/// Runs a fixture file named by `PAINLESS_ADHOC` (a path; relative paths resolve in
/// `tests/fixtures`) — for probing new captures before they get their own test.
#[test]
fn adhoc() {
    if let Ok(p) = std::env::var("PAINLESS_ADHOC") {
        check(&[&p]);
    }
}
