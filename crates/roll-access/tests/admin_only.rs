//! Every product refusal that says "needs the admin token" sits behind a request the catalogue
//! classifies admin (`src/admin-only.txt`): a scoped token reaches products as their operator, so
//! anything less lets a write or deploy token past the product's own check.

use roll_access::catalog;
use roll_auth::scope::Action;
use std::path::Path;

const MARK: &str = "needs the admin token";

/// Every string literal in a product's sources that holds [`MARK`].
fn refusals(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            refusals(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            for (i, line) in text.lines().enumerate() {
                let mut rest = line;
                while let Some(start) = rest.find('"') {
                    let after = &rest[start + 1..];
                    let Some(end) = after.find('"') else { break };
                    let lit = &after[..end];
                    if lit.contains(MARK) {
                        out.push((lit.to_string(), format!("{}:{}", p.display(), i + 1)));
                    }
                    rest = &after[end + 1..];
                }
            }
        }
    }
}

#[test]
fn every_admin_only_refusal_is_admin_in_the_catalogue() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let listed: Vec<(String, String, String, Option<serde_json::Value>)> = include_str!("../src/admin-only.txt")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let parts: Vec<&str> = l.split(" | ").collect();
            assert_eq!(parts.len(), 3, "admin-only.txt: `message | METHOD path | body`: {l}");
            let (method, path) = parts[1].split_once(' ').unwrap();
            let body = (parts[2] != "-").then(|| serde_json::from_str(parts[2]).unwrap_or_else(|e| panic!("admin-only.txt: bad JSON in {l}: {e}")));
            (parts[0].to_string(), method.to_string(), path.to_string(), body)
        })
        .collect();
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&root).unwrap().flatten() {
        let src = entry.path().join("src");
        if src.is_dir() && entry.file_name() != "roll-access" {
            refusals(&src, &mut found);
        }
    }
    assert!(!found.is_empty(), "no refusals found: is the search root right?");
    let missing: Vec<String> = found.iter().filter(|(m, _)| !listed.iter().any(|(l, ..)| l == m)).map(|(m, at)| format!("{at}: {m:?}")).collect();
    assert!(
        missing.is_empty(),
        "admin-only refusals with no request in crates/roll-access/src/admin-only.txt: classify the route (or its body, with Act::Body) admin in catalog.rs, then list a request that reaches each:\n{}",
        missing.join("\n")
    );
    for (message, method, path, body) in &listed {
        assert!(found.iter().any(|(m, _)| m == message), "admin-only.txt lists {message:?}, which no product says any more: remove it");
        let row = catalog::by_path(method, path).unwrap_or_else(|| panic!("{method} {path} ({message}) is in no catalogue row")).row;
        let action = row.action(method, &[], body.as_ref());
        assert_eq!(action, Action::Admin, "{method} {path} with {body:?} reaches {message:?} but the catalogue makes it {action:?}: classify it admin in catalog.rs");
    }
}
