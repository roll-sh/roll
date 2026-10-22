//! Results: per-test outcomes, the JSON report, the human summary and baseline comparison.
//!
//! A test id is `[<suite label>:]<dir>/<file without .yml>/<test name>`, the Java runner's
//! `getTestPath()` (what Elasticsearch's `muted-tests.yml` and `tests.rest.blacklist` use), with
//! an optional label when several suite roots run together (`free:search/10_basic/Basic`).
//!
//! A baseline is a text file of ids (one per line, `#` comments) that are expected to pass:
//! comparing a run against it yields *regressions* (in the baseline, not passing now) and *new
//! passes*. `crates/search-yaml/baseline/es-9.5.5.txt` lists what passes on a real
//! Elasticsearch 9.5.5, which is roll's target.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

/// Where in a test a failure happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The file couldn't be parsed.
    Parse,
    Setup,
    Test,
    Teardown,
    /// Wiping the cluster after the test.
    Cleanup,
    /// Creating a fresh cluster for the test.
    Cluster,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub phase: Phase,
    /// Line of the failing step in the test file.
    pub line: usize,
    /// The step kind (`do`, `match`, ...).
    pub step: String,
    /// For `do` failures, the API called.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// The request sent by the last `do` (`GET /index/_doc/1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// A short key shared by failures with the same root cause (the API and the error it
    /// returned, or the API whose response an assertion rejected); the summary ranks them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<Value>,
    /// Status line and the beginning of the last response body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub id: String,
    /// The suite label (when several roots run together).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub suite: String,
    pub area: String,
    pub file: String,
    pub name: String,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// A teardown step that failed. Like the Java runner (whose error collector is verified
    /// before teardown runs) this doesn't fail the test; it is reported for diagnosis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown_failure: Option<Failure>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl Totals {
    fn add(&mut self, s: Status) {
        self.total += 1;
        match s {
            Status::Pass => self.passed += 1,
            Status::Fail => self.failed += 1,
            Status::Skip => self.skipped += 1,
        }
    }
}

/// Differences against a baseline.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BaselineDiff {
    pub baseline: String,
    /// In the baseline and run, but not passing.
    pub regressions: Vec<String>,
    /// Passing but not in the baseline.
    pub new_passes: Vec<String>,
    /// In the baseline but not run (filtered out or missing from the suite), counting only the
    /// suites (id labels) this run ran.
    pub not_run: usize,
    /// Baseline tests run per area (the summary's `target` column).
    pub targets: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetSummary {
    pub url: String,
    /// The other `--url`s of the run (identical clusters, one worker each).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub more_urls: Vec<String>,
    pub flavor: String,
    pub version: String,
    pub os: String,
    /// How request bodies were encoded (`json`, a media type, or `random`).
    pub content_type: String,
    /// How paths and methods were chosen (`natural`, `first`, `random`).
    pub method_strategy: String,
    /// The seed of the random strategies, when one is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub target: TargetSummary,
    pub started: String,
    pub duration_ms: u64,
    pub totals: Totals,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineDiff>,
    pub tests: Vec<TestResult>,
}

impl Report {
    pub fn totals(tests: &[TestResult]) -> Totals {
        let mut t = Totals::default();
        for r in tests {
            t.add(r.status);
        }
        t
    }

    pub fn write_json(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }

    /// Writes a JUnit XML report: one `testsuite` per area (`[suite:]area`), one `testcase` per
    /// test (`classname` = the file's id), failures with the step, request, expected/actual and
    /// response excerpt in the body.
    pub fn write_junit(&self, path: &Path) -> anyhow::Result<()> {
        let mut by_area: BTreeMap<String, Vec<&TestResult>> = BTreeMap::new();
        for t in &self.tests {
            by_area.entry(area_key(t)).or_default().push(t);
        }
        let secs = |ms: u64| format!("{:.3}", ms as f64 / 1000.0);
        let mut out = String::with_capacity(self.tests.len() * 160);
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let t = &self.totals;
        let _ = writeln!(
            out,
            "<testsuites name=\"search-yaml\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{}\">",
            t.total,
            t.failed,
            t.skipped,
            secs(self.duration_ms)
        );
        for (area, tests) in &by_area {
            let mut totals = Totals::default();
            for t in tests {
                totals.add(t.status);
            }
            let time: u64 = tests.iter().map(|t| t.duration_ms).sum();
            let _ = writeln!(
                out,
                "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{}\">",
                xml_escape(area),
                totals.total,
                totals.failed,
                totals.skipped,
                secs(time)
            );
            for t in tests {
                let class = t.id.strip_suffix(t.name.as_str()).map(|c| c.trim_end_matches('/')).unwrap_or(&t.id);
                let _ = write!(
                    out,
                    "    <testcase classname=\"{}\" name=\"{}\" time=\"{}\"",
                    xml_escape(class),
                    xml_escape(&t.name),
                    secs(t.duration_ms)
                );
                match (t.status, &t.failure, &t.skip_reason) {
                    (Status::Fail, Some(f), _) => {
                        let first = f.message.lines().next().unwrap_or("");
                        let mut body = format!("{:?} line {}: {}\n", f.phase, f.line, f.step).to_lowercase();
                        if let Some(r) = &f.request {
                            let _ = writeln!(body, "request: {r}");
                        }
                        let _ = writeln!(body, "{}", f.message);
                        if let Some(e) = &f.expected {
                            let _ = writeln!(body, "expected: {}", e.to_json());
                        }
                        if let Some(a) = &f.actual {
                            let _ = writeln!(body, "actual: {}", a.to_json());
                        }
                        if let Some(r) = &f.response {
                            let _ = writeln!(body, "response: {r}");
                        }
                        let _ = writeln!(
                            out,
                            ">\n      <failure message=\"{}\" type=\"{}\">{}</failure>\n    </testcase>",
                            xml_escape(first),
                            xml_escape(f.cause.as_deref().unwrap_or(&f.step)),
                            xml_escape(&body)
                        );
                    }
                    (Status::Skip, _, reason) => {
                        let _ = writeln!(
                            out,
                            ">\n      <skipped message=\"{}\"/>\n    </testcase>",
                            xml_escape(reason.as_deref().unwrap_or(""))
                        );
                    }
                    _ => out.push_str("/>\n"),
                }
            }
            out.push_str("  </testsuite>\n");
        }
        out.push_str("</testsuites>\n");
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, out)?;
        Ok(())
    }

    /// Writes the ids of passing tests, sorted.
    pub fn write_passing(&self, path: &Path) -> anyhow::Result<()> {
        let mut ids: Vec<&str> = self.tests.iter().filter(|t| t.status == Status::Pass).map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        let mut out = String::with_capacity(ids.len() * 48);
        for id in ids {
            out.push_str(id);
            out.push('\n');
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, out)?;
        Ok(())
    }

    pub fn compare(&mut self, baseline_name: &str, baseline: &HashSet<String>) {
        let mut diff = BaselineDiff { baseline: baseline_name.to_owned(), ..Default::default() };
        // Ids are compared without trailing whitespace: test names may end in spaces, which
        // editors and `load_id_list` don't keep.
        let mut seen = HashSet::new();
        let mut labels = HashSet::new();
        for t in &self.tests {
            let id = t.id.trim_end();
            seen.insert(id);
            labels.insert(t.suite.as_str());
            let expected = baseline.contains(id);
            if expected {
                *diff.targets.entry(area_key(t)).or_default() += 1;
            }
            match (expected, t.status) {
                (true, Status::Pass) => {}
                (true, _) => diff.regressions.push(t.id.clone()),
                (false, Status::Pass) => diff.new_passes.push(t.id.clone()),
                _ => {}
            }
        }
        diff.not_run = baseline.iter().filter(|id| !seen.contains(id.as_str()) && labels.contains(id_label(id))).count();
        diff.regressions.sort();
        diff.new_passes.sort();
        self.baseline = Some(diff);
    }

    /// The human summary: totals per API area, then failures.
    pub fn summary(&self, max_failures: usize) -> String {
        let mut by_area: BTreeMap<String, Totals> = BTreeMap::new();
        for t in &self.tests {
            by_area.entry(area_key(t)).or_default().add(t.status);
        }
        let mut out = String::new();
        let width = by_area.keys().map(String::len).max().unwrap_or(4).clamp(4, 48);
        // With a baseline, `target` is how many of the area's tests pass on the reference.
        let targets = self.baseline.as_ref().map(|b| &b.targets);
        let _ = write!(out, "{:<width$}  {:>6} {:>6} {:>6} {:>6}", "area", "pass", "fail", "skip", "total");
        let _ = writeln!(out, "{}", if targets.is_some() { "  target" } else { "" });
        for (area, t) in &by_area {
            let _ = write!(out, "{:<width$}  {:>6} {:>6} {:>6} {:>6}", area, t.passed, t.failed, t.skipped, t.total);
            match targets {
                Some(m) => {
                    let _ = writeln!(out, "  {:>6}", m.get(area).copied().unwrap_or(0));
                }
                None => out.push('\n'),
            }
        }
        let failures: Vec<&TestResult> = self.tests.iter().filter(|t| t.status == Status::Fail).collect();
        if !failures.is_empty() {
            let _ = writeln!(out, "\nfailures ({}):", failures.len());
            for t in failures.iter().take(max_failures) {
                let f = t.failure.as_ref();
                let first = f.map(|f| f.message.lines().next().unwrap_or("").to_owned()).unwrap_or_default();
                let short: String = first.chars().take(240).collect();
                let at = f.map(|f| format!("{:?}:{} {}", f.phase, f.line, f.step).to_lowercase()).unwrap_or_default();
                let _ = writeln!(out, "  {}\n      [{}] {}", t.id, at, short);
            }
            if failures.len() > max_failures {
                let _ = writeln!(out, "  ... and {} more (see the JSON report)", failures.len() - max_failures);
            }
            let causes = top_causes(&failures);
            if causes.len() > 1 || causes.first().is_some_and(|c| c.1 > 1) {
                let _ = writeln!(out, "\ntop failure causes:");
                for (cause, n, example) in causes.iter().take(max_failures.min(30)) {
                    let _ = writeln!(out, "  {n:>5}  {cause}\n         e.g. {example}");
                }
                if causes.len() > 30.min(max_failures) {
                    let _ = writeln!(out, "  ... {} more causes", causes.len() - 30.min(max_failures));
                }
            }
        }
        if let Some(b) = &self.baseline {
            let _ = writeln!(
                out,
                "\nbaseline {}: {} regressions, {} new passes, {} baseline tests not run",
                b.baseline,
                b.regressions.len(),
                b.new_passes.len(),
                b.not_run
            );
            for id in b.regressions.iter().take(max_failures) {
                let _ = writeln!(out, "  regression: {id}");
            }
        }
        let t = &self.totals;
        let _ = writeln!(
            out,
            "\n{} tests: {} passed, {} failed, {} skipped in {:.1}s against {} {} ({}){}",
            t.total,
            t.passed,
            t.failed,
            t.skipped,
            self.duration_ms as f64 / 1000.0,
            self.target.flavor,
            self.target.version,
            match self.target.more_urls.len() {
                0 => self.target.url.clone(),
                n => format!("{} and {n} more", self.target.url),
            },
            match self.target.seed {
                Some(seed) => format!(", bodies: {}, paths/methods: {}, seed {seed}", self.target.content_type, self.target.method_strategy),
                None if self.target.content_type != "json" => format!(", bodies: {}", self.target.content_type),
                None => String::new(),
            }
        );
        out
    }
}

/// Escapes text for XML 1.0 attributes and content; characters XML 1.0 can't carry (most
/// control characters) become U+FFFD.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// The summary's grouping key: `[suite:]area`.
fn area_key(t: &TestResult) -> String {
    if t.suite.is_empty() { t.area.clone() } else { format!("{}:{}", t.suite, t.area) }
}

/// The suite label of an id (`free` in `free:search/10_basic/x`), empty without one. Labels never
/// contain `/`; test names may contain `:`.
fn id_label(id: &str) -> &str {
    match id.split_once(':') {
        Some((label, _)) if !label.contains('/') => label,
        _ => "",
    }
}

/// Failure causes by count (ties by name), each with an example test id.
fn top_causes<'a>(failures: &[&'a TestResult]) -> Vec<(String, usize, &'a str)> {
    let mut by_cause: BTreeMap<String, (usize, &str)> = BTreeMap::new();
    for t in failures {
        let Some(f) = &t.failure else { continue };
        let cause = match &f.cause {
            Some(c) => c.clone(),
            None => format!("{:?} {}", f.phase, f.step).to_lowercase(),
        };
        by_cause.entry(cause).or_insert((0, &t.id)).0 += 1;
    }
    let mut v: Vec<(String, usize, &str)> = by_cause.into_iter().map(|(c, (n, e))| (c, n, e)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Reads a baseline or id list: one id per line, `#` comments and blank lines ignored.
pub fn load_id_list(path: &Path) -> anyhow::Result<HashSet<String>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(str::to_owned)
        .collect())
}

/// The ids of the failed tests in a JSON report written by [`Report::write_json`] (to re-run
/// just those).
pub fn load_failed_ids(path: &Path) -> anyhow::Result<HashSet<String>> {
    let report = Value::from_json(&std::fs::read(path)?)?;
    let Some(tests) = report.get("tests").and_then(Value::as_list) else {
        anyhow::bail!("{} is not a search-yaml JSON report (no `tests`)", path.display());
    };
    Ok(tests
        .iter()
        .filter(|t| t.get("status").and_then(Value::as_str) == Some("fail"))
        .filter_map(|t| t.get("id").and_then(Value::as_str))
        .map(|id| id.trim_end().to_owned())
        .collect())
}

/// A skip list entry: tests matching `pattern` are skipped with `reason`.
#[derive(Debug, Clone)]
pub struct SkipEntry {
    pub pattern: String,
    pub reason: String,
}

/// Reads a skip list: `<glob>  # <reason>` per line (`*` matches anything, `?` one character).
pub fn load_skip_list(path: &Path) -> anyhow::Result<Vec<SkipEntry>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (pattern, reason) = match trimmed.split_once(" #") {
            Some((p, r)) => (p.trim(), r.trim()),
            None => (trimmed, "listed in the skip list"),
        };
        out.push(SkipEntry { pattern: pattern.to_owned(), reason: reason.to_owned() });
    }
    Ok(out)
}

/// Glob match: `*` any run of characters (including `/`), `?` any single character.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, status: Status) -> TestResult {
        TestResult {
            id: id.into(),
            suite: String::new(),
            area: id.split('/').next().unwrap().into(),
            file: String::new(),
            name: String::new(),
            status,
            skip_reason: None,
            failure: None,
            teardown_failure: None,
            duration_ms: 1,
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("search/*", "search/10_basic/Basic"));
        assert!(glob_match("*/10_basic/*", "search/10_basic/Basic"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exact2"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn baseline_comparison_and_summary() {
        let tests = vec![result("a/1/x", Status::Pass), result("a/1/y", Status::Fail), result("b/2/z", Status::Pass)];
        let mut report = Report {
            target: TargetSummary { url: "u".into(), more_urls: Vec::new(), flavor: "elasticsearch".into(), version: "9.5.5".into(), os: String::new(), content_type: "json".into(), method_strategy: "natural".into(), seed: None },
            started: String::new(),
            duration_ms: 1500,
            totals: Report::totals(&tests),
            baseline: None,
            tests,
        };
        let baseline: HashSet<String> = ["a/1/x", "a/1/y", "c/3/w"].iter().map(|s| s.to_string()).collect();
        report.compare("base", &baseline);
        let b = report.baseline.as_ref().unwrap();
        assert_eq!(b.regressions, ["a/1/y"]);
        assert_eq!(b.new_passes, ["b/2/z"]);
        assert_eq!(b.not_run, 1);
        let s = report.summary(10);
        assert!(s.contains("3 tests: 2 passed, 1 failed, 0 skipped in 1.5s"), "{s}");
        assert!(s.contains("regression: a/1/y"), "{s}");
        assert_eq!(b.targets.get("a"), Some(&2));
        assert!(s.contains("target"), "{s}");
    }

    #[test]
    fn baseline_ids_ignore_trailing_spaces_and_other_suites() {
        let mut tests = vec![result("a/1/name with a trailing space ", Status::Pass)];
        tests[0].suite = String::new();
        let mut report = Report {
            target: TargetSummary { url: "u".into(), more_urls: Vec::new(), flavor: "elasticsearch".into(), version: "9.5.5".into(), os: String::new(), content_type: "json".into(), method_strategy: "natural".into(), seed: None },
            started: String::new(),
            duration_ms: 0,
            totals: Report::totals(&tests),
            baseline: None,
            tests,
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("base.txt");
        std::fs::write(&p, "a/1/name with a trailing space \nclients:x/y/z\n").unwrap();
        report.compare("base", &load_id_list(&p).unwrap());
        let b = report.baseline.as_ref().unwrap();
        assert!(b.new_passes.is_empty() && b.regressions.is_empty(), "{b:?}");
        // `clients:` ids belong to a suite this run didn't include.
        assert_eq!(b.not_run, 0);
        assert_eq!(id_label("free:search/10_basic/a: b"), "free");
        assert_eq!(id_label("search/10_basic/a: b"), "");
    }

    #[test]
    fn junit_and_failed_ids() {
        let mut tests = vec![result("a/1/ok", Status::Pass), result("a/1/bad <&>", Status::Fail), result("b/2/skipped", Status::Skip)];
        tests[0].name = "ok".into();
        tests[1].name = "bad <&>".into();
        tests[1].failure = Some(Failure {
            phase: Phase::Test,
            line: 7,
            step: "match".into(),
            api: None,
            request: Some("GET /x".into()),
            cause: Some("search: match mismatch".into()),
            message: "field [a] does not match\u{1}\nsecond line".into(),
            expected: Some(Value::Int(1)),
            actual: Some(Value::Str("q".into())),
            response: None,
        });
        tests[2].skip_reason = Some(r#"needs "two" nodes"#.into());
        let report = Report {
            target: TargetSummary { url: "u".into(), more_urls: Vec::new(), flavor: "elasticsearch".into(), version: "9.5.5".into(), os: String::new(), content_type: "json".into(), method_strategy: "natural".into(), seed: None },
            started: String::new(),
            duration_ms: 1234,
            totals: Report::totals(&tests),
            baseline: None,
            tests,
        };
        let dir = tempfile::tempdir().unwrap();
        let x = dir.path().join("junit.xml");
        report.write_junit(&x).unwrap();
        let xml = std::fs::read_to_string(&x).unwrap();
        assert!(xml.contains(r#"<testsuites name="search-yaml" tests="3" failures="1" skipped="1" time="1.234">"#), "{xml}");
        assert!(xml.contains(r#"<testsuite name="a" tests="2" failures="1" skipped="0""#), "{xml}");
        assert!(xml.contains(r#"<testcase classname="a/1" name="ok" time="0.001"/>"#), "{xml}");
        assert!(xml.contains(r#"name="bad &lt;&amp;&gt;""#), "{xml}");
        assert!(
            xml.contains("<failure message=\"field [a] does not match\u{FFFD}\" type=\"search: match mismatch\">test line 7: match"),
            "{xml}"
        );
        assert!(xml.contains("request: GET /x\n"), "{xml}");
        assert!(xml.contains("actual: &quot;q&quot;"), "{xml}");
        assert!(xml.contains(r#"<skipped message="needs &quot;two&quot; nodes"/>"#), "{xml}");

        let j = dir.path().join("report.json");
        report.write_json(&j).unwrap();
        let failed = load_failed_ids(&j).unwrap();
        assert_eq!(failed.into_iter().collect::<Vec<_>>(), ["a/1/bad <&>"]);
        assert!(load_failed_ids(&x).is_err());
    }

    #[test]
    fn causes_are_ranked() {
        let mut tests = Vec::new();
        for (id, cause) in [("a/1/x", "index → 400 x"), ("a/1/y", "index → 400 x"), ("b/1/z", "search: match mismatch")] {
            let mut t = result(id, Status::Fail);
            t.failure = Some(Failure {
                phase: Phase::Test,
                line: 1,
                step: "do".into(),
                api: None,
                request: None,
                cause: Some(cause.into()),
                message: "m".into(),
                expected: None,
                actual: None,
                response: None,
            });
            tests.push(t);
        }
        let refs: Vec<&TestResult> = tests.iter().collect();
        let causes = top_causes(&refs);
        assert_eq!(causes[0], ("index → 400 x".to_owned(), 2, "a/1/x"));
        assert_eq!(causes[1].1, 1);
        let report = Report {
            target: TargetSummary { url: "u".into(), more_urls: Vec::new(), flavor: "elasticsearch".into(), version: "9.5.5".into(), os: String::new(), content_type: "json".into(), method_strategy: "natural".into(), seed: None },
            started: String::new(),
            duration_ms: 0,
            totals: Report::totals(&tests),
            baseline: None,
            tests,
        };
        let s = report.summary(10);
        assert!(s.contains("top failure causes:\n      2  index → 400 x\n         e.g. a/1/x"), "{s}");
    }

    #[test]
    fn list_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("skips.txt");
        std::fs::write(&p, "# header\nsearch/*/Slow test  # needs two nodes\n\nplain/id\n").unwrap();
        let skips = load_skip_list(&p).unwrap();
        assert_eq!(skips.len(), 2);
        assert_eq!(skips[0].pattern, "search/*/Slow test");
        assert_eq!(skips[0].reason, "needs two nodes");
        let ids = load_id_list(&p).unwrap();
        assert!(ids.contains("plain/id"));
        assert_eq!(ids.len(), 2);
    }
}
