//! Replays the golden files in `tests/golden/` against the in-memory backend and against the
//! engine backend (a real database per file, tables stored as ScyllaDB stores them).
//!
//! Each file is a script of DynamoDB requests and the exact responses ScyllaDB 2026.3.3 gave
//! (captured with `tests/golden/capture.py`). Every request goes through the full HTTP handler;
//! status and body must match, where `"*"` in an expected body accepts any value. Object member
//! order is ignored; array order is not.
//!
//! `GOLDEN=<substring>` limits the run to matching file names; `GOLDEN_VERBOSE=1` prints every
//! mismatch instead of the first few per file.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use nosql_alternator::http::{HttpRequest, HttpResponse};
use nosql_alternator::memory::MemoryBackend;
use nosql_alternator::{Alternator, Config};
use serde_json::Value;

const VOLATILE: &[&str] = &[
    "CreationDateTime",
    "LastUpdateToPayPerRequestDateTime",
    "LastIncreaseDateTime",
    "LastDecreaseDateTime",
    "TableId",
    "LatestStreamArn",
    "LatestStreamLabel",
    "StreamArn",
    "StreamLabel",
    "ShardIterator",
    "NextShardIterator",
    "SequenceNumber",
    "StartingSequenceNumber",
    "EndingSequenceNumber",
    "ApproximateCreationDateTime",
    "eventID",
    "ShardId",
    "ParentShardId",
    "LastEvaluatedShardId",
    "LastEvaluatedStreamArn",
    "CreationRequestDateTime",
    "TableSizeBytes",
    "IndexSizeBytes",
    "ItemCount",
];

/// Whether `actual` matches `expected`, where `"*"` matches anything.
fn matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::String(s), _) if s == "*" => true,
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| matches(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, x)| b.get(k).is_some_and(|y| matches(x, y)))
        }
        _ => expected == actual,
    }
}

/// The first place where `actual` differs from `expected`.
fn diff(expected: &Value, actual: &Value, path: &str) -> Option<String> {
    let short = |v: &Value| v.to_string().chars().take(160).collect::<String>();
    match (expected, actual) {
        (Value::String(s), _) if s == "*" => None,
        (Value::Number(a), Value::Number(b)) if a.as_f64() == b.as_f64() => None,
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{path}: {} elements, got {}", a.len(), b.len()));
            }
            a.iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (x, y))| diff(x, y, &format!("{path}[{i}]")))
        }
        (Value::Object(a), Value::Object(b)) => {
            for (k, x) in a {
                match b.get(k) {
                    None => return Some(format!("{path}.{k}: missing (want {})", short(x))),
                    Some(y) => {
                        if let Some(d) = diff(x, y, &format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                }
            }
            b.keys()
                .find(|k| !a.contains_key(*k))
                .map(|k| format!("{path}.{k}: unexpected {}", short(&b[k])))
        }
        _ if expected == actual => None,
        _ => Some(format!(
            "{path}: want {}, got {}",
            short(expected),
            short(actual)
        )),
    }
}

fn harvest(v: &Value, seen: &mut HashMap<String, String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if VOLATILE.contains(&k.as_str()) {
                    match x {
                        Value::String(s) => {
                            seen.insert(k.clone(), s.clone());
                        }
                        Value::Number(n) => {
                            seen.insert(k.clone(), n.to_string());
                        }
                        _ => {}
                    }
                }
                harvest(x, seen);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| harvest(x, seen)),
        _ => {}
    }
}

/// Expands `$repeat(TEXT,COUNT)` and `$Name`.
fn substitute(body: &str, seen: &HashMap<String, String>) -> String {
    let mut s = body.to_string();
    while let Some(i) = s.find("$repeat(") {
        let rest = &s[i + 8..];
        let Some(comma) = rest.find(',') else { break };
        let Some(close) = rest.find(')') else { break };
        let text = &rest[..comma];
        let count: usize = rest[comma + 1..close].parse().unwrap_or(0);
        let expanded = text.repeat(count);
        s = format!("{}{}{}", &s[..i], expanded, &rest[close + 1..]);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '$' {
            let name: String = s[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .collect();
            if let Some(v) = seen.get(&name) {
                out.push_str(v);
                for _ in 0..name.len() {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

struct Mismatch {
    file: String,
    line: usize,
    request: String,
    expected: String,
    actual: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Memory,
    Engine,
}

async fn replay(path: &std::path::Path, kind: Kind) -> (usize, Vec<Mismatch>) {
    let text = std::fs::read_to_string(path).unwrap();
    let file = path.file_name().unwrap().to_string_lossy().to_string();
    let mut _store = None;
    let backend: Arc<dyn nosql_alternator::Backend> = match kind {
        Kind::Memory => Arc::new(MemoryBackend::new()),
        Kind::Engine => {
            let (db, store) = common::engine_db("golden").await;
            _store = Some(store);
            Arc::new(nosql_alternator::engine::EngineBackend::new(db))
        }
    };
    let alt = Alternator::new(backend, Config::default());
    let mut seen = HashMap::new();
    let mut mismatches = Vec::new();
    let mut cases = 0;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some(secs) = line.strip_prefix("! sleep ") {
            // Time moves forward without waiting.
            let s: f64 = secs.trim().parse().unwrap_or(0.0);
            alt.advance_clock((s * 1000.0) as i64);
            continue;
        }
        let Some(req) = line.strip_prefix("> ") else {
            continue;
        };
        let (op, body) = req.split_once(' ').unwrap_or((req, ""));
        if std::env::var("GOLDEN_TRACE").is_ok() {
            eprintln!(
                "{}:{} {}",
                file,
                i,
                req.chars().take(120).collect::<String>()
            );
        }
        let body = substitute(body, &seen);
        let mut headers = vec![(
            "content-type".to_string(),
            "application/x-amz-json-1.0".to_string(),
        )];
        if op != "-" {
            headers.push(("x-amz-target".into(), format!("DynamoDB_20120810.{op}")));
        }
        let resp: HttpResponse = alt
            .handle(HttpRequest {
                method: "POST".into(),
                path: "/".into(),
                query: String::new(),
                headers,
                body: body.into_bytes().into(),
                peer: None,
            })
            .await;
        let actual_text = String::from_utf8_lossy(&resp.body).to_string();
        let actual: Value = serde_json::from_str(&actual_text)
            .unwrap_or_else(|_| Value::String(actual_text.clone()));
        harvest(&actual, &mut seen);
        let Some(expect_line) = lines.get(i).and_then(|l| l.strip_prefix("< ")) else {
            continue;
        };
        i += 1;
        cases += 1;
        let (status, expected) = expect_line.split_once(' ').unwrap_or((expect_line, "null"));
        let expected: Value = serde_json::from_str(expected).unwrap_or(Value::Null);
        if status != resp.status.to_string() || !matches(&expected, &actual) {
            let mut actual_line = format!(
                "{} {}",
                resp.status,
                actual_text.chars().take(600).collect::<String>()
            );
            if status == resp.status.to_string()
                && let Some(d) = diff(&expected, &actual, "")
            {
                actual_line = format!("{} [{d}]", resp.status);
            }
            mismatches.push(Mismatch {
                file: file.clone(),
                line: i - 1,
                request: req.chars().take(400).collect(),
                expected: expect_line.chars().take(600).collect(),
                actual: actual_line,
            });
        }
    }
    (cases, mismatches)
}

#[tokio::test]
async fn golden() {
    run(Kind::Memory, "known_differences.txt").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn golden_engine() {
    run(Kind::Engine, "known_differences_engine.txt").await;
}

async fn run(kind: Kind, known_file: &str) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let filter = std::env::var("GOLDEN").unwrap_or_default();
    let verbose = std::env::var("GOLDEN_VERBOSE").is_ok();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "txt"))
        .filter(|p| {
            !p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("known_differences")
        })
        .filter(|p| p.file_name().unwrap().to_string_lossy().contains(&filter))
        .collect();
    files.sort();
    // Known differences: "<reason> <file>:<line>".
    let known_text = std::fs::read_to_string(dir.join(known_file)).unwrap_or_default();
    let mut known: HashMap<String, String> = known_text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            l.split_once(' ')
                .map(|(r, loc)| (loc.trim().to_string(), r.to_string()))
        })
        .collect();
    let (mut total, mut failed, mut expected) = (0, 0, 0);
    let mut summary = Vec::new();
    for f in &files {
        let (cases, mut mismatches) = replay(f, kind).await;
        total += cases;
        let before = mismatches.len();
        mismatches.retain(|m| known.remove(&format!("{}:{}", m.file, m.line)).is_none());
        expected += before - mismatches.len();
        failed += mismatches.len();
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        summary.push(format!(
            "{name}: {}/{cases}",
            cases - mismatches.len() - (before - mismatches.len())
        ));
        for m in mismatches.iter().take(if verbose { usize::MAX } else { 3 }) {
            eprintln!(
                "--- {}:{}\n  > {}\n  want < {}\n  got  < {}",
                m.file, m.line, m.request, m.expected, m.actual
            );
        }
    }
    eprintln!("{}", summary.join("\n"));
    eprintln!(
        "golden: {}/{} responses match ({expected} known differences, see {known_file})",
        total - failed - expected,
        total
    );
    assert_eq!(failed, 0, "{failed} of {total} golden responses differ");
    // Entries of files that ran but whose responses now match are stale.
    let stale: Vec<&String> = known
        .keys()
        .filter(|loc| {
            files
                .iter()
                .any(|f| loc.starts_with(&*f.file_name().unwrap().to_string_lossy()))
        })
        .collect();
    assert!(
        stale.is_empty(),
        "known differences that now match (remove them): {stale:?}"
    );
}
