//! The index over a small real repo: build, incremental updates, every query.

use async_trait::async_trait;
use code_index::{Index, Query, Source, Update};
use std::collections::HashMap;
use std::sync::Mutex;

struct Files(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait]
impl Source for Files {
    async fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(path).cloned()
    }
}

fn repo() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/parse.rs", "pub fn parse_header(input: &str) -> Header {\n    let n = input.len();\n    Header { len: n }\n}\n\npub struct Header {\n    pub len: usize,\n}\n"),
        ("src/server.rs", "use crate::parse::parse_header;\n\npub fn handle(req: &str) {\n    let h = parse_header(req);\n    log_header(&h);\n}\n\nfn log_header(h: &Header) {}\n"),
        ("src/client.rs", "pub fn send(body: &str) {\n    let h = parse_header(body);\n    let again = parse_header(body);\n}\n"),
        ("tests/parse.rs", "#[test]\nfn parses() {\n    assert_eq!(parse_header(\"ab\").len, 2);\n}\n"),
        ("web/app.ts", "export function render(header: Header) {\n  return header.len;\n}\n"),
        ("README.md", "# Demo\n\nCall parse_header to parse a header.\n"),
        ("logo.png", "\u{0}\u{1}PNG"),
    ]
}

async fn build(dir: &std::path::Path) -> (Index, Files) {
    let idx = Index::open(dir).unwrap();
    let files = Files(Mutex::new(repo().into_iter().map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())).collect()));
    let r = idx.repo("repo_1").unwrap();
    let updates: Vec<Update> = repo().into_iter().map(|(p, c)| Update { path: p.into(), content: Some(c.as_bytes().to_vec()) }).collect();
    r.apply(Some("cs_1"), &updates).unwrap();
    (idx, files)
}

#[tokio::test]
async fn queries() {
    let dir = tempfile::tempdir().unwrap();
    let (idx, files) = build(dir.path()).await;
    let r = idx.repo("repo_1").unwrap();
    assert_eq!(r.head().unwrap().as_deref(), Some("cs_1"));
    let st = r.stats().unwrap();
    assert_eq!(st.files, 6, "the PNG isn't indexed");
    assert!(st.symbols >= 5 && st.trigrams > 100);

    let d = r.definitions("parse_header", 10).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].path.as_str(), d[0].line, d[0].kind.as_str()), ("src/parse.rs", 1, "function"));
    assert_eq!(r.definitions("Header", 10).unwrap()[0].kind, "type");

    let refs = r.references("parse_header", 10).unwrap();
    let paths: Vec<&str> = refs.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(refs[0].path, "src/client.rs", "most uses first: {refs:?}");
    assert_eq!(refs[0].lines, vec![2, 3]);
    for p in ["src/server.rs", "tests/parse.rs", "README.md", "src/parse.rs"] {
        assert!(paths.contains(&p), "{p}: {paths:?}");
    }
    assert_eq!(r.reference_count("parse_header").unwrap(), 5);

    let callers = r.callers_of(&["parse_header".into()], &["src/parse.rs".into()], 10).unwrap();
    assert_eq!(callers[0].0, "parse_header");
    assert!(!callers[0].1.iter().any(|h| h.path == "src/parse.rs"));

    let tests = r.related_tests(&["src/parse.rs".into()], &["parse_header".into()], 5).unwrap();
    assert_eq!(tests, vec!["tests/parse.rs"]);

    let sim = r.similar("pub fn parse_header(input: &str) -> Header {\n    let n = input.len();", &["src/parse.rs".into()], 3).unwrap();
    assert!(!sim.is_empty());
    assert!(sim.iter().all(|(p, s)| p != "src/parse.rs" && *s > 0.0 && *s <= 1.0));
    assert!(r.similar("ab", &[], 3).unwrap().is_empty());

    // Literal search (case-insensitive by default), confirmed on content.
    let all = |_: &str| true;
    let q = Query { text: "PARSE_HEADER(".into(), regex: false, case_sensitive: false, limit: 50 };
    let res = r.search(&q, &files, &all).await.unwrap();
    let hits: Vec<(String, u64)> = res.matches.iter().map(|m| (m.path.clone(), m.line)).collect();
    assert!(hits.contains(&("src/client.rs".into(), 2)) && hits.contains(&("src/client.rs".into(), 3)));
    assert!(hits.contains(&("src/parse.rs".into(), 1)));
    assert!(!hits.iter().any(|(p, _)| p == "web/app.ts"));
    assert_eq!(res.head.as_deref(), Some("cs_1"));
    // Case-sensitive misses the upper-case query.
    let cs = Query { case_sensitive: true, ..q.clone() };
    assert!(r.search(&cs, &files, &all).await.unwrap().matches.is_empty());
    // Regex, with a path filter.
    let re = Query { text: r"fn\s+\w+_header".into(), regex: true, case_sensitive: true, limit: 50 };
    let only_src = |p: &str| p.starts_with("src/");
    let res = r.search(&re, &files, &only_src).await.unwrap();
    let hits: Vec<&str> = res.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(hits, ["src/parse.rs", "src/server.rs"]);
    // A regex without literals scans everything; a limit truncates.
    let any = Query { text: r"\w+".into(), regex: true, case_sensitive: false, limit: 3 };
    let res = r.search(&any, &files, &all).await.unwrap();
    assert_eq!(res.matches.len(), 3);
    assert!(res.truncated);
    assert!(r.search(&Query { text: "(".into(), regex: true, case_sensitive: false, limit: 3 }, &files, &all).await.is_err());
    assert_eq!(r.paths().unwrap().len(), 6);
}

#[tokio::test]
async fn incremental_updates_and_removal() {
    let dir = tempfile::tempdir().unwrap();
    let (idx, files) = build(dir.path()).await;
    let r = idx.repo("repo_1").unwrap();
    let before = r.stats().unwrap();
    // Rename the function in parse.rs, delete client.rs, add a new caller.
    let parse2 = "pub fn read_header(input: &str) -> Header {\n    Header { len: input.len() }\n}\n";
    let new = "fn main() { read_header(\"x\"); }\n";
    r.apply(
        Some("cs_2"),
        &[
            Update { path: "src/parse.rs".into(), content: Some(parse2.as_bytes().to_vec()) },
            Update { path: "src/client.rs".into(), content: None },
            Update { path: "src/main.rs".into(), content: Some(new.as_bytes().to_vec()) },
        ],
    )
    .unwrap();
    {
        let mut f = files.0.lock().unwrap();
        f.insert("src/parse.rs".into(), parse2.as_bytes().to_vec());
        f.remove("src/client.rs");
        f.insert("src/main.rs".into(), new.as_bytes().to_vec());
    }
    assert_eq!(r.head().unwrap().as_deref(), Some("cs_2"));
    assert!(r.definitions("parse_header", 5).unwrap().is_empty());
    assert_eq!(r.definitions("read_header", 5).unwrap()[0].path, "src/parse.rs");
    let refs: Vec<String> = r.references("read_header", 10).unwrap().into_iter().map(|h| h.path).collect();
    assert_eq!(refs.len(), 2);
    assert!(refs.contains(&"src/main.rs".to_string()));
    assert!(!r.references("parse_header", 10).unwrap().iter().any(|h| h.path == "src/client.rs"));
    let q = Query { text: "read_header".into(), regex: false, case_sensitive: true, limit: 10 };
    let res = r.search(&q, &files, &|_: &str| true).await.unwrap();
    assert_eq!(res.matches.len(), 2);
    // Deleting a file removes its trigram postings too.
    let after = r.stats().unwrap();
    assert_eq!(after.files, before.files);
    let (cands, _) = r.candidates(&Query { text: "again".into(), regex: false, case_sensitive: false, limit: 1 }).unwrap();
    assert!(cands.is_empty(), "client.rs's trigrams are gone: {cands:?}");
    // Persisted, and reopened by another Index.
    drop(r);
    let idx2 = Index::open(dir.path()).unwrap();
    assert_eq!(idx2.repo("repo_1").unwrap().head().unwrap().as_deref(), Some("cs_2"));
    idx2.repo("repo_1").unwrap().clear().unwrap();
    assert_eq!(idx2.repo("repo_1").unwrap().stats().unwrap().files, 0);
    idx.remove("repo_1");
    assert!(!dir.path().join("repo_1.db").exists());
    assert!(idx.repo("../x").is_err());
}
