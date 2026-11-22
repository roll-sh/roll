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

/// The files whose routers declare roll code's routes.
const ROUTERS: [&str; 11] = [
    "crates/code-control/src/api.rs",
    "crates/code-control/src/review/api.rs",
    "crates/code-control/src/ai/api.rs",
    "crates/code-control/src/agents/api.rs",
    "crates/code-control/src/agents/sandbox/api.rs",
    "crates/code-control/src/chatops/api.rs",
    "crates/code-control/src/agents/automations/api.rs",
    "crates/code-control/src/agents/linear/api.rs",
    "crates/code-control/src/agents/lookup/mod.rs",
    "crates/code-control/src/mcp.rs",
    "crates/code-git/src/server.rs",
];

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
    for f in ROUTERS {
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
    assert_eq!(tools.len(), 73);
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

/// Every dashboard or desktop test file the map names (`src/…/x.test.tsx`, relative to its app or
/// the repo, or a bare file name under the dashboard's `src/`) exists.
#[test]
fn every_web_test_file_the_map_names_exists() {
    fn names(dir: &Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                names(&p, out);
            } else if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                out.insert(n.to_owned());
            }
        }
    }
    let mut bare = BTreeSet::new();
    names(&root().join("apps/dashboard/src"), &mut bare);
    let doc = read("docs/code/test-coverage.md");
    let mut checked = 0;
    let mut wrong = Vec::new();
    for cap in doc.split('`').skip(1).step_by(2) {
        if !(cap.ends_with(".test.ts") || cap.ends_with(".test.tsx")) || cap.contains('*') || cap.contains(' ') {
            continue;
        }
        checked += 1;
        let found = if cap.contains('/') {
            ["", "apps/dashboard", "apps/desktop"].iter().any(|base| root().join(base).join(cap).exists())
        } else {
            bare.contains(cap)
        };
        if !found {
            wrong.push(cap.to_owned());
        }
    }
    assert!(checked > 50, "checked only {checked} references");
    assert!(wrong.is_empty(), "docs/code/test-coverage.md names web test files that don't exist:\n  {}", wrong.join("\n  "));
}

/// Every `/v1/code` route the routers declare is an MCP tool's (`mcp::ROUTES`) or in
/// `crates/code-control/src/no-tool.txt` with why, method by method, as roll-mcp holds roll's other
/// products: a new route can't land undecided.
#[test]
fn every_route_has_an_mcp_tool_or_a_reason() {
    let norm = |p: &str| p.split('/').map(|s| if s.starts_with('{') { "{}" } else { s }).collect::<Vec<_>>().join("/").trim_end_matches('/').to_string();
    let declared: BTreeSet<(String, String)> = ROUTERS
        .iter()
        .flat_map(|f| routes(&read(f)))
        .filter_map(|r| {
            let (m, p) = r.split_once(' ')?;
            (p == "/v1/code" || p.starts_with("/v1/code/")).then(|| (m.to_string(), norm(p)))
        })
        .collect();
    assert!(declared.len() > 150, "found only {} /v1/code routes: is the parser still right?", declared.len());
    let names: Vec<String> = code_control::mcp::tools().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    let table = code_control::mcp::ROUTES;
    let silent: Vec<&String> = names.iter().filter(|n| !table.iter().any(|r| r.0 == n.as_str())).collect();
    assert!(silent.is_empty(), "tools with no mcp::ROUTES row: {silent:?}");
    let unknown: Vec<&str> = table.iter().map(|r| r.0).filter(|t| !names.iter().any(|n| n == t)).collect();
    assert!(unknown.is_empty(), "mcp::ROUTES rows for tools that don't exist: {unknown:?}");
    let invented: Vec<String> = table.iter().filter(|(_, m, p)| !declared.contains(&(m.to_string(), norm(p)))).map(|(t, m, p)| format!("{t}: {m} {p}")).collect();
    assert!(invented.is_empty(), "mcp::ROUTES rows no router declares (a wrong path or method):\n{}", invented.join("\n"));
    let covered = |m: &str, p: &str| table.iter().any(|(_, tm, tp)| *tm == m && norm(tp) == p);
    let doc = read("crates/code-control/src/no-tool.txt");
    let listed: Vec<(String, String)> = doc
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some((w.next()?.to_string(), norm(w.next()?)))
        })
        .collect();
    let undecided: Vec<String> = declared.iter().filter(|(m, p)| !covered(m, p) && !listed.contains(&(m.clone(), p.clone()))).map(|(m, p)| format!("{m} {p}")).collect();
    assert!(undecided.is_empty(), "/v1/code routes with no MCP tool and no reason: add the tool (and its mcp::ROUTES row), or `METHOD route  why` to crates/code-control/src/no-tool.txt:\n{}", undecided.join("\n"));
    let stale: Vec<String> = listed.iter().filter(|(m, p)| covered(m, p)).map(|(m, p)| format!("{m} {p}")).collect();
    assert!(stale.is_empty(), "no-tool.txt lines for routes that have a tool now: remove them:\n{}", stale.join("\n"));
    let gone: Vec<String> = listed.iter().filter(|r| !declared.contains(r)).map(|(m, p)| format!("{m} {p}")).collect();
    assert!(gone.is_empty(), "no-tool.txt lines for routes no router declares: remove them:\n{}", gone.join("\n"));
}
