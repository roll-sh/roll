//! docs/code/test-coverage.md maps every feature to its tests. This keeps the map complete:
//! every route the API serves, every MCP tool and every event kind has a row there that names at
//! least one test, and every test the map names exists. The CLI's commands are checked the same
//! way in `roll-cli` (`main.rs`, `every_code_command_is_in_the_coverage_map`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(p: &str) -> String {
    std::fs::read_to_string(root().join(p)).unwrap_or_else(|e| panic!("{p}: {e}"))
}

/// `METHOD /path` for every `.route(…)` in a router source file.
fn routes(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let src = src.replace("axum::routing::", "");
    for line in src.lines().map(str::trim).filter(|l| l.starts_with(".route(") || l.contains("Router::new().route(")) {
        let rest = &line[line.find(".route(").unwrap() + ".route(".len()..];
        let (path, handlers) = if let Some(r) = rest.strip_prefix("&repo(\"") {
            let end = r.find('"').unwrap();
            (format!("/v1/code/repos/{{owner}}/{{repo}}{}", &r[..end]), &r[end..])
        } else {
            let r = rest.strip_prefix('"').unwrap_or_else(|| panic!("unexpected route line: {line}"));
            let end = r.find('"').unwrap();
            (r[..end].to_string(), &r[end..])
        };
        let mut found = false;
        for m in ["get", "post", "put", "patch", "delete"] {
            for (i, _) in handlers.match_indices(&format!("{m}(")) {
                let before = handlers[..i].chars().last();
                if matches!(before, Some(' ' | '.' | ',' | '(')) {
                    out.insert(format!("{} {path}", m.to_uppercase()));
                    found = true;
                }
            }
        }
        assert!(found, "no method found in: {line}");
    }
    out
}

/// Table rows of the map: (first column, the rest).
fn rows(doc: &str) -> Vec<(String, String)> {
    doc.lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| ---"))
        .filter_map(|l| {
            let cells: Vec<&str> = l.trim_matches('|').split(" | ").collect();
            (cells.len() >= 2).then(|| (cells[0].trim().to_string(), cells[1..].join(" | ")))
        })
        .collect()
}

/// The row whose first column names `key` in backticks, with at least one test (`file::fn`).
fn has_row(rows: &[(String, String)], key: &str) -> bool {
    let quoted = format!("`{key}`");
    rows.iter().any(|(first, rest)| first.split(" / ").any(|k| k.trim() == quoted || k.trim().starts_with(&format!("{quoted} "))) && rest.contains("::"))
}

#[test]
fn every_route_tool_and_event_has_tests_in_the_map() {
    let doc = read("docs/code/test-coverage.md");
    let rows = rows(&doc);
    let mut missing = Vec::new();

    let mut all = BTreeSet::new();
    for f in ["crates/code-control/src/api.rs", "crates/code-control/src/review/api.rs", "crates/code-control/src/ai/api.rs", "crates/code-control/src/mcp.rs", "crates/code-git/src/server.rs"] {
        all.extend(routes(&read(f)));
    }
    assert!(all.len() > 100, "found only {} routes: is the parser still right?", all.len());
    assert!(all.contains("PATCH /v1/code/changes/{id}/enqueue") && all.contains("GET /git/{*path}") && all.contains("DELETE /v1/code/mcp"));
    for r in &all {
        if !has_row(&rows, r) {
            missing.push(format!("route {r}"));
        }
    }
    let tools: Vec<String> = code_control::mcp::tools().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(tools.len(), 22);
    for t in &tools {
        if !has_row(&rows, t) {
            missing.push(format!("MCP tool {t}"));
        }
    }
    for e in code_control::webhooks::EVENTS {
        if !rows.iter().any(|(first, rest)| first.starts_with(&format!("`{e}`")) && rest.contains("::")) {
            missing.push(format!("event {e}"));
        }
    }
    assert!(missing.is_empty(), "docs/code/test-coverage.md has no row with tests for:\n  {}", missing.join("\n  "));
}

/// The files a reference like `file.rs::name` can mean: a path under crates/, or a bare file
/// name in any crate's tests/ or src/ (the map names code-control's tests and well-known files of
/// other crates that way).
fn candidates(file: &str) -> Vec<PathBuf> {
    if file.contains('/') {
        let p = root().join("crates").join(file);
        return if p.exists() { vec![p] } else { Vec::new() };
    }
    let mut out = Vec::new();
    for c in std::fs::read_dir(root().join("crates")).unwrap().flatten() {
        for dir in ["tests", "src"] {
            let p = c.path().join(dir).join(file);
            if p.exists() {
                out.push(p);
            }
        }
    }
    out
}

/// Every `file.rs::test_name` the map names exists as a function in that file.
#[test]
fn every_test_the_map_names_exists() {
    let doc = read("docs/code/test-coverage.md");
    let mut checked = 0;
    let mut wrong = Vec::new();
    for cap in doc.split('`').skip(1).step_by(2) {
        let Some((file, name)) = cap.split_once(".rs::") else { continue };
        if name.contains(' ') || name.contains("::") || name.is_empty() {
            continue;
        }
        let file = format!("{file}.rs");
        let candidates = candidates(&file);
        if candidates.is_empty() {
            wrong.push(format!("{file}::{name} (no such file)"));
            continue;
        }
        checked += 1;
        if !candidates.iter().any(|p| std::fs::read_to_string(p).is_ok_and(|src| src.contains(&format!("fn {name}(")))) {
            wrong.push(format!("{file}::{name}"));
        }
    }
    assert!(checked > 200, "checked only {checked} references");
    assert!(wrong.is_empty(), "docs/code/test-coverage.md names tests that don't exist:\n  {}", wrong.join("\n  "));
}
