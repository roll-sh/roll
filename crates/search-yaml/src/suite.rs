//! Test files: parsing YAML documents into setup, teardown and test sections.
//!
//! The structure follows Elasticsearch's `ClientYamlTestSuite`: every `---` document is a map whose
//! key is `setup`, `teardown` or a test name and whose value is a list of steps. A section may start
//! with `skip`/`requires` prerequisites. Files from `elasticsearch-clients-tests` add a top-level
//! `requires: {serverless, stack}` document, which is kept as [`Suite::clients_requires`].
//!
//! `do` parameters keep their source text (`parser.text()` in Java), bodies are typed values, and
//! a string body is a stream of JSON documents (an NDJSON `bulk` body written as a block scalar).
//! The parser is strict where the Java runner is strict (unknown steps, malformed sections), so a
//! file Elasticsearch's runner rejects is reported as a file error rather than half-run.

use std::path::{Path, PathBuf};

use crate::value::{Map, Value};
use crate::yaml::{Node, NodeKind, load_documents};

/// One YAML test file.
#[derive(Debug, Clone)]
pub struct Suite {
    pub file: PathBuf,
    /// Path relative to the suite root without `.yml`, with `/` separators: `search/10_basic`.
    pub id: String,
    /// The directory part of `id` (the API area): `search`, `indices.create`, `` for root files.
    pub area: String,
    pub setup: Section,
    pub teardown: Section,
    pub tests: Vec<TestSection>,
    /// `requires: {serverless, stack, feature_flag}` from elasticsearch-clients-tests.
    pub clients_requires: Option<ClientsRequires>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientsRequires {
    pub serverless: Option<bool>,
    pub stack: Option<bool>,
    pub feature_flag: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Section {
    pub prereq: Prereq,
    pub steps: Vec<Step>,
}

impl Section {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.prereq.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct TestSection {
    pub name: String,
    pub line: usize,
    pub prereq: Prereq,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub line: usize,
    pub kind: StepKind,
}

#[derive(Debug, Clone)]
pub enum StepKind {
    Do(Box<DoStep>),
    /// `set: {response.path: stash_name}` (in file order).
    Set(Vec<(String, String)>),
    /// `transform_and_set: {stash_name: "#base64EncodeCredentials(a,b)"}`.
    TransformAndSet(Vec<(String, String)>),
    Assert(Assertion),
}

impl StepKind {
    pub fn name(&self) -> &'static str {
        match self {
            StepKind::Do(_) => "do",
            StepKind::Set(_) => "set",
            StepKind::TransformAndSet(_) => "transform_and_set",
            StepKind::Assert(a) => a.op.name(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Assertion {
    pub op: AssertOp,
    /// The response path (or `$stash` reference) being checked.
    pub field: String,
    /// The expected value (unused for `is_true`/`is_false`/`exists`/`not_exists`).
    pub expected: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssertOp {
    Match,
    Length,
    IsTrue,
    IsFalse,
    Gt,
    Gte,
    Lt,
    Lte,
    Contains,
    CloseTo { error: f64 },
    MatchesInAnyOrder,
    IsAfter,
    Exists,
    NotExists,
}

impl AssertOp {
    pub fn name(&self) -> &'static str {
        match self {
            AssertOp::Match => "match",
            AssertOp::Length => "length",
            AssertOp::IsTrue => "is_true",
            AssertOp::IsFalse => "is_false",
            AssertOp::Gt => "gt",
            AssertOp::Gte => "gte",
            AssertOp::Lt => "lt",
            AssertOp::Lte => "lte",
            AssertOp::Contains => "contains",
            AssertOp::CloseTo { .. } => "close_to",
            AssertOp::MatchesInAnyOrder => "matches_in_any_order",
            AssertOp::IsAfter => "is_after",
            AssertOp::Exists => "exists",
            AssertOp::NotExists => "not_exists",
        }
    }
}

/// A request body.
#[derive(Debug, Clone)]
pub enum Body {
    /// A typed document, serialized in the request's media type at send time (after unstashing).
    /// String bodies are parsed as JSON when the file is loaded, as the Java runner does.
    Value(Value),
}

#[derive(Debug, Clone, Default)]
pub struct DoStep {
    pub api: String,
    pub method: Option<String>,
    /// Parameters with their source text; list values are joined with `,`.
    pub params: Vec<(String, String)>,
    pub bodies: Vec<Body>,
    /// Headers (names case-insensitively unique, last wins).
    pub headers: Vec<(String, String)>,
    pub catch: Option<String>,
    pub warnings: Vec<String>,
    pub warnings_regex: Vec<String>,
    pub allowed_warnings: Vec<String>,
    pub allowed_warnings_regex: Vec<String>,
    /// A `node_selector` was given (ignored: one node, or a load balancer in front of many).
    pub node_selector: bool,
}

impl DoStep {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// `requires` + `skip` of a section.
#[derive(Debug, Clone, Default)]
pub struct Prereq {
    pub skip_reason: Option<String>,
    pub requires_reason: Option<String>,
    /// Legacy `skip.version` ranges (`" - 8.0.99"`, `"all"`, `"8.1.0 - 8.3.99, 8.5.0 - "`).
    pub skip_version: Option<String>,
    pub skip_os: Vec<String>,
    pub skip_cluster_features: Vec<String>,
    pub skip_all_nodes: bool,
    pub skip_capabilities: Vec<CapabilityCheck>,
    pub skip_known_issues: Vec<(String, String)>,
    pub awaits_fix: Option<String>,
    /// `test_runner_features` (and legacy `skip.features`), minus `xpack`/`no_xpack`.
    pub runner_features: Vec<String>,
    pub xpack: XPackRequirement,
    pub required_cluster_features: Vec<String>,
    pub required_capabilities: Vec<CapabilityCheck>,
}

impl Prereq {
    pub fn is_empty(&self) -> bool {
        self.skip_version.is_none()
            && self.skip_os.is_empty()
            && self.skip_cluster_features.is_empty()
            && self.skip_capabilities.is_empty()
            && self.skip_known_issues.is_empty()
            && self.awaits_fix.is_none()
            && self.runner_features.is_empty()
            && self.xpack == XPackRequirement::Unspecified
            && self.required_cluster_features.is_empty()
            && self.required_capabilities.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum XPackRequirement {
    #[default]
    Unspecified,
    Required,
    Forbidden,
}

/// A `capabilities` entry: checked with `GET /_capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CapabilityCheck {
    pub method: String,
    pub path: String,
    /// Comma-joined, or `None`.
    pub parameters: Option<String>,
    pub capabilities: Option<String>,
}

/// A file that couldn't be parsed.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

fn perr<T>(line: usize, message: impl Into<String>) -> Result<T> {
    Err(ParseError { line, message: message.into() })
}

/// Computes `(id, area)` of a file below a suite root.
pub fn suite_id(root: &Path, file: &Path) -> (String, String) {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let mut parts: Vec<String> = rel.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    if let Some(last) = parts.last_mut()
        && let Some(stem) = last.strip_suffix(".yml").or_else(|| last.strip_suffix(".yaml"))
    {
        *last = stem.to_owned();
    }
    let area = parts[..parts.len().saturating_sub(1)].join("/");
    (parts.join("/"), area)
}

impl Suite {
    /// Reads and parses a test file.
    pub fn load(root: &Path, file: &Path) -> std::result::Result<Suite, ParseError> {
        let text = std::fs::read_to_string(file).map_err(|e| ParseError { line: 0, message: e.to_string() })?;
        let (id, area) = suite_id(root, file);
        Suite::parse(&text, file.to_path_buf(), id, area)
    }

    /// Parses the text of a test file.
    pub fn parse(text: &str, file: PathBuf, id: String, area: String) -> Result<Suite> {
        let docs = load_documents(text).map_err(|e| ParseError { line: e.line, message: e.message })?;
        let mut suite = Suite {
            file,
            id,
            area,
            setup: Section::default(),
            teardown: Section::default(),
            tests: Vec::new(),
            clients_requires: None,
        };
        let mut seen_setup = false;
        let mut seen_teardown = false;
        for doc in &docs {
            let Some(entries) = doc.as_map() else {
                return perr(doc.line, "expected token to be START_OBJECT");
            };
            for (key, value) in entries {
                let name = key.key_text();
                match name.as_str() {
                    "setup" if value.as_seq().is_some() || value.is_null() => {
                        if seen_setup {
                            return perr(key.line, "duplicate setup section");
                        }
                        seen_setup = true;
                        suite.setup = parse_section(value, "setup")?;
                    }
                    "teardown" if value.as_seq().is_some() || value.is_null() => {
                        if seen_teardown {
                            return perr(key.line, "duplicate teardown section");
                        }
                        seen_teardown = true;
                        suite.teardown = parse_section(value, "teardown")?;
                    }
                    "requires" if value.as_map().is_some() => {
                        suite.clients_requires = Some(parse_clients_requires(value)?);
                    }
                    _ => {
                        let section = parse_section(value, &name)
                            .map_err(|e| ParseError { line: e.line, message: format!("Error parsing test named [{name}]: {}", e.message) })?;
                        if suite.tests.iter().any(|t| t.name == name) {
                            return perr(key.line, format!("duplicate test section [{name}]"));
                        }
                        if section.steps.is_empty() {
                            return perr(key.line, format!("No executable sections loaded for [{name}]"));
                        }
                        suite.tests.push(TestSection { name, line: key.line, prereq: section.prereq, steps: section.steps });
                    }
                }
            }
        }
        Ok(suite)
    }

    /// The id of one of the file's tests: `search/10_basic/Basic search`.
    pub fn test_id(&self, test: &TestSection) -> String {
        format!("{}/{}", self.id, test.name)
    }
}

fn parse_clients_requires(node: &Node) -> Result<ClientsRequires> {
    let mut r = ClientsRequires::default();
    for (k, v) in node.as_map().unwrap_or_default() {
        match k.key_text().as_str() {
            "serverless" => r.serverless = v.to_value().as_bool(),
            "stack" => r.stack = v.to_value().as_bool(),
            "feature_flag" => r.feature_flag = v.text().map(str::to_owned),
            other => return perr(k.line, format!("unknown field [{other}] in requires section")),
        }
    }
    Ok(r)
}

fn parse_section(node: &Node, name: &str) -> Result<Section> {
    let mut section = Section::default();
    let items: &[Node] = match &node.kind {
        NodeKind::Seq(items) => items,
        NodeKind::Scalar { .. } if node.is_null() => &[],
        _ => return perr(node.line, format!("section [{name}] must be a list of steps")),
    };
    let mut leading = true;
    let mut has_prereq = false;
    for item in items {
        let Some(entries) = item.as_map() else {
            return perr(item.line, "expected a step (a map with one key)");
        };
        for (k, v) in entries {
            let key = k.key_text();
            if leading && (key == "skip" || key == "requires") {
                has_prereq = true;
                if key == "skip" {
                    parse_skip(v, &mut section.prereq)?;
                } else {
                    parse_requires(v, &mut section.prereq)?;
                }
                continue;
            }
            leading = false;
            let kind = parse_step(&key, v, k.line)?;
            if name == "teardown" && !matches!(kind, StepKind::Do(_)) {
                return perr(k.line, format!("section [{key}] not supported within teardown section"));
            }
            section.steps.push(Step { line: k.line, kind });
        }
    }
    if has_prereq {
        validate_prereq(&section.prereq, node.line)?;
    }
    Ok(section)
}

/// `PrerequisiteSectionBuilder.validate`, run when a section has `skip`/`requires` (blank strings
/// count as absent, as `Strings.isEmpty`).
fn validate_prereq(p: &Prereq, line: usize) -> Result<()> {
    let blank = |s: &Option<String>| s.as_deref().is_none_or(str::is_empty);
    let no_skip_criteria = blank(&p.skip_version)
        && p.skip_os.is_empty()
        && p.skip_cluster_features.is_empty()
        && p.skip_capabilities.is_empty()
        && p.skip_known_issues.is_empty();
    let no_requires_criteria = p.required_cluster_features.is_empty() && p.required_capabilities.is_empty();
    if no_skip_criteria
        && no_requires_criteria
        && blank(&p.awaits_fix)
        && p.xpack == XPackRequirement::Unspecified
        && p.runner_features.is_empty()
    {
        return perr(line, "at least one predicate is mandatory within a skip or requires section");
    }
    if blank(&p.skip_reason) && !no_skip_criteria {
        return perr(line, "reason is mandatory within this skip section");
    }
    if blank(&p.requires_reason) && !no_requires_criteria {
        return perr(line, "reason is mandatory within this requires section");
    }
    if !p.skip_os.is_empty() && !p.runner_features.iter().any(|f| f == "skip_os") {
        return perr(line, "if os is specified, test runner feature [skip_os] must be set");
    }
    if p.skip_cluster_features.iter().any(|f| p.required_cluster_features.contains(f)) {
        return perr(line, "a cluster feature can be specified either in [requires] or [skip], not both");
    }
    Ok(())
}

/// A string or a list of strings.
fn strings(node: &Node) -> Result<Vec<String>> {
    match &node.kind {
        NodeKind::Scalar { text, .. } => Ok(vec![text.clone()]),
        NodeKind::Seq(items) => items
            .iter()
            .map(|i| i.text().map(str::to_owned).ok_or(ParseError { line: i.line, message: "expected a string".into() }))
            .collect(),
        NodeKind::Map(_) => perr(node.line, "expected a string or a list of strings"),
    }
}

/// Runner feature names: a string or a list; a string may hold several, comma-separated, as
/// OpenSearch's runner reads it (`features: "headers, warnings"`).
fn feature_names(node: &Node) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for s in strings(node)? {
        out.extend(s.split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned));
    }
    Ok(out)
}

fn add_feature(p: &mut Prereq, feature: String) -> Result<()> {
    match feature.as_str() {
        "xpack" => {
            p.xpack = match p.xpack {
                XPackRequirement::Forbidden => return perr(0, "either [xpack] or [no_xpack] can be present, not both"),
                _ => XPackRequirement::Required,
            }
        }
        "no_xpack" => {
            p.xpack = match p.xpack {
                XPackRequirement::Required => return perr(0, "either [xpack] or [no_xpack] can be present, not both"),
                _ => XPackRequirement::Forbidden,
            }
        }
        _ => p.runner_features.push(feature),
    }
    Ok(())
}

fn parse_capabilities(node: &Node) -> Result<Vec<CapabilityCheck>> {
    let Some(items) = node.as_seq() else {
        return perr(node.line, "[capabilities] must be a list");
    };
    let mut out = Vec::new();
    for item in items {
        let Some(entries) = item.as_map() else {
            return perr(item.line, "a capabilities check must be an object");
        };
        let mut check = CapabilityCheck { method: "GET".into(), path: String::new(), parameters: None, capabilities: None };
        let mut has_path = false;
        for (k, v) in entries {
            let joined = || -> Result<String> { Ok(strings(v)?.join(",")) };
            match k.key_text().as_str() {
                "method" => check.method = v.text().unwrap_or("GET").to_owned(),
                "path" => {
                    has_path = true;
                    check.path = v.text().unwrap_or_default().to_owned();
                }
                "parameters" => check.parameters = Some(joined()?),
                "capabilities" => check.capabilities = Some(joined()?),
                other => {
                    return perr(k.line, format!("Expected some of [method, path, parameters, capabilities], but got [{other}]"));
                }
            }
        }
        if !has_path {
            return perr(item.line, "path is required");
        }
        out.push(check);
    }
    Ok(out)
}

fn parse_skip(node: &Node, p: &mut Prereq) -> Result<()> {
    let Some(entries) = node.as_map() else {
        return perr(node.line, "Expected [START_OBJECT], the skip section is not properly indented");
    };
    for (k, v) in entries {
        match k.key_text().as_str() {
            "reason" => p.skip_reason = v.text().map(str::to_owned),
            "version" => p.skip_version = v.text().map(str::to_owned),
            "features" => {
                for f in feature_names(v)? {
                    add_feature(p, f).map_err(|e| ParseError { line: k.line, ..e })?;
                }
            }
            "os" => p.skip_os.extend(strings(v)?),
            "cluster_features" => p.skip_cluster_features.extend(strings(v)?),
            "awaits_fix" => p.awaits_fix = v.text().map(str::to_owned),
            "all_nodes" => p.skip_all_nodes = v.to_value().as_bool().unwrap_or(false),
            "known_issues" => {
                let Some(items) = v.as_seq() else {
                    return perr(v.line, "[known_issues] must be a list");
                };
                for item in items {
                    let cf = item.get("cluster_feature").and_then(Node::text);
                    let fb = item.get("fixed_by").and_then(Node::text);
                    match (cf, fb, item.as_map().map(<[_]>::len)) {
                        (Some(cf), Some(fb), Some(2)) => p.skip_known_issues.push((cf.to_owned(), fb.to_owned())),
                        _ => return perr(item.line, "Expected all of [cluster_feature, fixed_by]"),
                    }
                }
            }
            "capabilities" => p.skip_capabilities.extend(parse_capabilities(v)?),
            other => return perr(k.line, format!("field [{other}] not supported within skip section")),
        }
    }
    Ok(())
}

fn parse_requires(node: &Node, p: &mut Prereq) -> Result<()> {
    let Some(entries) = node.as_map() else {
        return perr(node.line, "Expected [START_OBJECT], the requires section is not properly indented");
    };
    for (k, v) in entries {
        match k.key_text().as_str() {
            "reason" => p.requires_reason = v.text().map(str::to_owned),
            "test_runner_features" => {
                for f in feature_names(v)? {
                    add_feature(p, f).map_err(|e| ParseError { line: k.line, ..e })?;
                }
            }
            "cluster_features" => p.required_cluster_features.extend(strings(v)?),
            "capabilities" => p.required_capabilities.extend(parse_capabilities(v)?),
            other => return perr(k.line, format!("field [{other}] not supported within requires section")),
        }
    }
    Ok(())
}

/// A single `{field: value}` entry (`ParserUtils.parseTuple`).
fn tuple<'a>(op: &str, node: &'a Node) -> Result<(String, &'a Node)> {
    match node.as_map() {
        Some([(k, v)]) => Ok((k.key_text(), v)),
        Some(_) => perr(node.line, format!("[{op}] expects exactly one {{field: value}} pair")),
        None => perr(node.line, format!("expected an object for [{op}]")),
    }
}

fn parse_step(key: &str, v: &Node, line: usize) -> Result<StepKind> {
    let assertion = |op: AssertOp| -> Result<StepKind> {
        let (field, value) = tuple(key, v)?;
        Ok(StepKind::Assert(Assertion { op, field, expected: value.to_value() }))
    };
    let field_only = |op: AssertOp| -> Result<StepKind> {
        let field = match &v.kind {
            NodeKind::Scalar { text, .. } => text.clone(),
            _ => return perr(v.line, format!("[{key}] expects a field name")),
        };
        Ok(StepKind::Assert(Assertion { op, field, expected: Value::Null }))
    };
    match key {
        "do" => Ok(StepKind::Do(Box::new(parse_do(v)?))),
        "set" | "transform_and_set" => {
            let Some(entries) = v.as_map() else {
                return perr(v.line, format!("[{key}] expects an object"));
            };
            let pairs: Vec<(String, String)> = entries
                .iter()
                .filter(|(_, val)| !val.is_null())
                .filter_map(|(k, val)| val.text().map(|t| (k.key_text(), t.to_owned())))
                .collect();
            if pairs.is_empty() {
                return perr(line, format!("{key} section must set at least a value"));
            }
            Ok(if key == "set" { StepKind::Set(pairs) } else { StepKind::TransformAndSet(pairs) })
        }
        "match" => assertion(AssertOp::Match),
        "length" => {
            // `LengthAssertion.parse`: a number (truncated to int) or a string holding one,
            // converted once here, so a stashed value can't be used.
            let (field, value) = tuple(key, v)?;
            let n = match value.to_value() {
                Value::Int(i) => i as i32 as i128,
                Value::Float(f) => f as i32 as i128,
                other => match other.java_string().parse::<i32>() {
                    Ok(n) => n as i128,
                    Err(_) => return perr(value.line, "length is not a valid number"),
                },
            };
            Ok(StepKind::Assert(Assertion { op: AssertOp::Length, field, expected: Value::Int(n) }))
        }
        "gt" | "gte" | "lt" | "lte" => {
            let (field, value) = tuple(key, v)?;
            let expected = value.to_value();
            if matches!(expected, Value::Map(_) | Value::List(_) | Value::Null) {
                let class = match expected {
                    Value::Map(_) => "LinkedHashMap",
                    Value::List(_) => "ArrayList",
                    _ => "null",
                };
                return perr(value.line, format!("{key} section can only be used with objects that support natural ordering, found {class}"));
            }
            let op = match key {
                "gt" => AssertOp::Gt,
                "gte" => AssertOp::Gte,
                "lt" => AssertOp::Lt,
                _ => AssertOp::Lte,
            };
            Ok(StepKind::Assert(Assertion { op, field, expected }))
        }
        "contains" => assertion(AssertOp::Contains),
        "matches_in_any_order" => assertion(AssertOp::MatchesInAnyOrder),
        "is_after" => assertion(AssertOp::IsAfter),
        "close_to" => {
            let (field, value) = tuple(key, v)?;
            let map = value.to_value();
            let Some(m) = map.as_map() else {
                return perr(value.line, "expected a map with value and error");
            };
            if m.len() != 2 {
                return perr(value.line, format!("expected a map with value and error but got a map with {} fields", m.len()));
            }
            let Some(expected) = m.get("value").filter(|v| !v.is_null()).cloned() else {
                return perr(value.line, "value is missing");
            };
            let Some(error) = m.get("error").and_then(Value::as_f64) else {
                return perr(value.line, "error is missing or not a number");
            };
            Ok(StepKind::Assert(Assertion { op: AssertOp::CloseTo { error }, field, expected }))
        }
        "is_true" => field_only(AssertOp::IsTrue),
        "is_false" => field_only(AssertOp::IsFalse),
        "exists" => field_only(AssertOp::Exists),
        "not_exists" => field_only(AssertOp::NotExists),
        other => perr(line, format!("unknown section [{other}]")),
    }
}

fn parse_do(node: &Node) -> Result<DoStep> {
    let Some(entries) = node.as_map() else {
        return perr(node.line, "expected [START_OBJECT], the do section is not properly indented");
    };
    let mut step = DoStep::default();
    let mut have_api = false;
    for (k, v) in entries {
        let key = k.key_text();
        // Jackson's VALUE_NULL is not a value for the Java parser: null entries are ignored.
        if v.is_null() {
            continue;
        }
        match key.as_str() {
            "catch" => step.catch = v.text().map(str::to_owned),
            "warnings" | "warnings_regex" | "allowed_warnings" | "allowed_warnings_regex" => {
                let Some(items) = v.as_seq() else {
                    return perr(v.line, format!("[{key}] must be a string array"));
                };
                let mut list = Vec::with_capacity(items.len());
                for i in items {
                    match i.text() {
                        Some(t) => list.push(t.to_owned()),
                        None => return perr(i.line, format!("[{key}] must be a string array")),
                    }
                }
                match key.as_str() {
                    "warnings" => step.warnings = list,
                    "warnings_regex" => step.warnings_regex = list,
                    "allowed_warnings" => step.allowed_warnings = list,
                    _ => step.allowed_warnings_regex = list,
                }
            }
            "headers" => {
                for (hk, hv) in v.as_map().unwrap_or_default() {
                    if hv.is_null() {
                        continue;
                    }
                    let name = hk.key_text();
                    let value = hv.text().unwrap_or_default().to_owned();
                    step.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
                    step.headers.push((name, value));
                }
            }
            "node_selector" => step.node_selector = true,
            api => {
                // A second API in the same `do` replaces the first, as in the Java parser.
                have_api = true;
                step.api = api.to_owned();
                step.params.clear();
                step.bodies.clear();
                step.method = None;
                match &v.kind {
                    NodeKind::Map(params) => {
                        for (pk, pv) in params {
                            if pv.is_null() {
                                continue;
                            }
                            let pname = pk.key_text();
                            match pname.as_str() {
                                "body" => collect_bodies(pv, &mut step.bodies)?,
                                "method" => step.method = pv.text().map(str::to_owned),
                                _ => {
                                    let text = match &pv.kind {
                                        NodeKind::Scalar { text, .. } => text.clone(),
                                        NodeKind::Seq(items) => items
                                            .iter()
                                            .map(|i| i.text().map(str::to_owned).unwrap_or_else(|| i.to_value().to_json()))
                                            .collect::<Vec<_>>()
                                            .join(","),
                                        NodeKind::Map(_) => {
                                            return perr(pv.line, format!("parameter [{pname}] must be a value or a list of values"));
                                        }
                                    };
                                    match step.params.iter_mut().find(|(n, _)| *n == pname) {
                                        Some((_, existing)) => {
                                            existing.push(',');
                                            existing.push_str(&text);
                                        }
                                        None => step.params.push((pname, text)),
                                    }
                                }
                            }
                        }
                    }
                    _ => return perr(v.line, format!("unsupported field [{api}]")),
                }
            }
        }
    }
    if !have_api {
        return perr(node.line, "client call section is mandatory within a do section");
    }
    for w in &step.warnings {
        if step.allowed_warnings.contains(w) {
            return perr(node.line, format!("the warning [{w}] was both allowed and expected"));
        }
    }
    for w in &step.warnings_regex {
        if step.allowed_warnings_regex.contains(w) {
            return perr(node.line, format!("the warning pattern [{w}] was both allowed and expected"));
        }
    }
    Ok(step)
}

fn collect_bodies(node: &Node, out: &mut Vec<Body>) -> Result<()> {
    match &node.kind {
        NodeKind::Map(_) => out.push(Body::Value(node.to_value())),
        NodeKind::Seq(items) => {
            for item in items {
                collect_bodies(item, out)?;
            }
        }
        NodeKind::Scalar { text, .. } => {
            // A null isn't a value token to Jackson: no body.
            if node.is_null() {
                return Ok(());
            }
            // DoSection: `while (bodyParser.nextToken() != null) addBody(bodyParser.mapOrdered())`
            // over the string with Elasticsearch's JSON parser (comments allowed, duplicates
            // rejected). Text that isn't JSON fails the file instead of being sent raw.
            let bad = |e: String| ParseError { line: node.line, message: format!("failed to parse the request body as JSON: {e}") };
            let values = Value::from_json_stream(&strip_json_comments(text).map_err(bad)?).map_err(|e| bad(e.to_string()))?;
            for v in values {
                string_body_tokens(v, out);
            }
        }
    }
    Ok(())
}

/// The bodies `mapOrdered()` yields for one top-level JSON value of a string body: an object is
/// a body; every other token (a scalar, an array's start and end) reads as an empty map, and an
/// array's elements are visited in turn.
fn string_body_tokens(v: Value, out: &mut Vec<Body>) {
    match v {
        Value::Map(_) => out.push(Body::Value(v)),
        Value::List(items) => {
            out.push(Body::Value(Value::Map(Map::new())));
            for item in items {
                string_body_tokens(item, out);
            }
            out.push(Body::Value(Value::Map(Map::new())));
        }
        _ => out.push(Body::Value(Value::Map(Map::new()))),
    }
}

/// Removes `//` and `/* */` comments outside strings (Jackson's `ALLOW_COMMENTS`, which
/// Elasticsearch's JSON parser enables); each becomes a space so tokens stay apart.
fn strip_json_comments(text: &str) -> std::result::Result<std::borrow::Cow<'_, str>, String> {
    if !text.contains('/') {
        return Ok(std::borrow::Cow::Borrowed(text));
    }
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut start, mut in_string) = (0, 0, false);
    while i < b.len() {
        match b[i] {
            b'\\' if in_string => i += 2,
            b'"' => {
                in_string = !in_string;
                i += 1;
            }
            b'/' if !in_string && matches!(b.get(i + 1), Some(b'/' | b'*')) => {
                out.push_str(&text[start..i]);
                out.push(' ');
                if b[i + 1] == b'/' {
                    i = text[i..].find(['\n', '\r']).map_or(b.len(), |n| i + n);
                } else {
                    let Some(n) = text[i + 2..].find("*/") else {
                        return Err("Unexpected end-of-input in a comment".into());
                    };
                    i += 2 + n + 2;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[start.min(text.len())..]);
    Ok(std::borrow::Cow::Owned(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::json;

    fn parse(text: &str) -> Suite {
        Suite::parse(text, PathBuf::from("x.yml"), "area/x".into(), "area".into()).unwrap()
    }

    const FILE: &str = r#"---
setup:
  - requires:
      cluster_features: ["gte_v8.0.0"]
      reason: needs it
  - do:
      indices.create:
        index: test
        body:
          settings: { number_of_shards: 1 }
---
teardown:
  - do:
      indices.delete: { index: test, ignore: 404 }
---
"Basic":
  - skip:
      features: "headers, xpack"
      version: " - 7.99.99"
      reason: old
  - do:
      catch: missing
      headers: { Content-Type: application/json, accept: "text/plain" }
      get:
        index: test
        id: 01
        stored_fields: [a, b]
        refresh: true
  - match: { error.type: /not_found/ }
  - set: { _id: id, _seq_no: seq }
  - is_true: ''
  - is_false: found
  - length: { hits.hits: 2 }
  - close_to: { took: { value: 0.5, error: 0.1 } }
---
"Bulk":
  - do:
      bulk:
        refresh: true
        body: |
          {"index": {"_index": "test"}}
          {"f": 1}
  - do:
      bulk:
        body:
          - index: { _index: test }
          - f: 2
          - '{"index": {}}'
"#;

    #[test]
    fn parses_sections_and_steps() {
        let s = parse(FILE);
        assert_eq!(s.setup.prereq.required_cluster_features, ["gte_v8.0.0"]);
        assert_eq!(s.setup.steps.len(), 1);
        assert_eq!(s.teardown.steps.len(), 1);
        assert_eq!(s.tests.len(), 2);
        let basic = &s.tests[0];
        assert_eq!(basic.name, "Basic");
        assert_eq!(basic.prereq.runner_features, ["headers"]);
        assert_eq!(basic.prereq.xpack, XPackRequirement::Required);
        assert_eq!(basic.prereq.skip_version.as_deref(), Some(" - 7.99.99"));
        let StepKind::Do(d) = &basic.steps[0].kind else { panic!() };
        assert_eq!(d.api, "get");
        assert_eq!(d.catch.as_deref(), Some("missing"));
        assert_eq!(d.param("id"), Some("01"), "raw text, not the octal int");
        assert_eq!(d.param("stored_fields"), Some("a,b"));
        assert_eq!(d.param("refresh"), Some("true"));
        assert_eq!(d.headers.len(), 2);
        assert_eq!(basic.steps[0].line, 21);
        let StepKind::Set(pairs) = &basic.steps[2].kind else { panic!() };
        assert_eq!(pairs, &[("_id".to_owned(), "id".to_owned()), ("_seq_no".to_owned(), "seq".to_owned())]);
        let StepKind::Assert(a) = &basic.steps[3].kind else { panic!() };
        assert_eq!((a.op.clone(), a.field.as_str()), (AssertOp::IsTrue, ""));
        let StepKind::Assert(a) = &basic.steps[6].kind else { panic!() };
        assert_eq!(a.op, AssertOp::CloseTo { error: 0.1 });
        assert_eq!(a.expected, json("0.5"));
        assert_eq!(s.test_id(basic), "area/x/Basic");
    }

    #[test]
    fn bodies_from_strings_and_lists() {
        let s = parse(FILE);
        let bulk = &s.tests[1];
        let StepKind::Do(d) = &bulk.steps[0].kind else { panic!() };
        assert_eq!(d.bodies.len(), 2);
        let StepKind::Do(d) = &bulk.steps[1].kind else { panic!() };
        assert_eq!(d.bodies.len(), 3);
        let Body::Value(v) = &d.bodies[2];
        assert_eq!(v, &json(r#"{"index":{}}"#));
    }

    fn bodies_of(yaml_body: &str) -> Result<Vec<String>> {
        let text = format!("\"t\":\n  - do:\n      index:\n        index: i\n        body: {yaml_body}\n");
        let s = Suite::parse(&text, PathBuf::from("x.yml"), "area/x".into(), "area".into())?;
        let StepKind::Do(d) = &s.tests[0].steps[0].kind else { panic!() };
        Ok(d.bodies.iter().map(|Body::Value(v)| v.to_json()).collect())
    }

    #[test]
    fn string_bodies_parse_like_the_java_runner() {
        assert_eq!(bodies_of(r#"'{"a": 1} {"b": 2}'"#).unwrap(), [r#"{"a":1}"#, r#"{"b":2}"#]);
        // Comments are allowed (ALLOW_COMMENTS), also inside the stream; not inside strings.
        assert_eq!(bodies_of(r#"'{"a": "x//y" /* c */ } // tail'"#).unwrap(), [r#"{"a":"x//y"}"#]);
        assert_eq!(bodies_of(r#"'{"a": "\"//"}'"#).unwrap(), [r#"{"a":"\"//"}"#]);
        // mapOrdered() reads every non-object token as an empty map.
        assert_eq!(bodies_of("'123'").unwrap(), ["{}"]);
        assert_eq!(bodies_of(r#"'[{"a":1}]'"#).unwrap(), ["{}", r#"{"a":1}"#, "{}"]);
        assert!(bodies_of("''").unwrap().is_empty());
        assert!(bodies_of("~").unwrap().is_empty());
        // Not JSON, or a repeated field: the file fails to parse.
        let e = bodies_of("'not json'").unwrap_err();
        assert!(e.message.contains("failed to parse the request body as JSON"), "{}", e.message);
        let e = bodies_of(r#"'{"f": 1, "f": 2}'"#).unwrap_err();
        assert!(e.message.contains("Duplicate field 'f'"), "{}", e.message);
        let e = bodies_of("{ f: 1, f: 2 }").unwrap_err();
        assert!(e.message.contains("Duplicate field 'f'"), "{}", e.message);
        assert!(bodies_of("'/* open'").is_err());
    }

    #[test]
    fn clients_tests_requires_document() {
        let s = parse("---\nrequires:\n  serverless: false\n  stack: true\n---\n\"t\":\n  - do: { info: {} }\n");
        let r = s.clients_requires.unwrap();
        assert_eq!((r.serverless, r.stack), (Some(false), Some(true)));
        assert_eq!(s.tests.len(), 1);
    }

    #[test]
    fn rejects_malformed_files() {
        let bad = [
            "\"t\":\n  - nope: {}\n",
            "\"t\":\n  - do: { catch: missing }\n",
            "\"t\":\n  - skip: { os: debian, reason: x }\n  - do: { info: {} }\n",
            "\"t\":\n  - do: { info: {} }\n\"t\":\n  - do: { info: {} }\n",
            "teardown:\n  - match: { a: 1 }\n",
            "\"t\":\n  - do: { info: {}, warnings: [a], allowed_warnings: [a] }\n",
            "\"t\":\n  - do: { info: ~ }\n",
            "\"t\":\n  - close_to: { a: { value: 1 } }\n",
            "\"t\":\n  - length: { a: $n }\n",
            "\"t\":\n  - length: { a: x }\n",
            "\"t\":\n  - gt: { a: [1] }\n",
            "\"t\":\n  - lte: { a: {b: 1} }\n",
        ];
        for text in bad {
            assert!(Suite::parse(text, PathBuf::new(), String::new(), String::new()).is_err(), "{text}");
        }
    }

    #[test]
    fn prerequisite_sections_are_validated_like_java() {
        let strip = |m: String| m.strip_prefix("Error parsing test named [t]: ").map(str::to_owned).unwrap_or(m);
        let err = |text: &str| strip(Suite::parse(text, PathBuf::new(), String::new(), String::new()).unwrap_err().message);
        let ok = |text: &str| Suite::parse(text, PathBuf::new(), String::new(), String::new()).map(|_| ()).map_err(|e| strip(e.message));
        let t = |prereq: &str| format!("\"t\":\n{prereq}  - do: {{ info: {{}} }}\n");
        assert_eq!(err(&t("  - requires: {}\n")), "at least one predicate is mandatory within a skip or requires section");
        assert_eq!(err(&t("  - skip: { reason: r }\n")), "at least one predicate is mandatory within a skip or requires section");
        assert_eq!(err(&t("  - skip: { version: \" - 7.0.0\" }\n")), "reason is mandatory within this skip section");
        assert_eq!(err(&t("  - skip: { version: \" - 7.0.0\", reason: \"\" }\n")), "reason is mandatory within this skip section");
        assert_eq!(err(&t("  - skip: { cluster_features: [f] }\n")), "reason is mandatory within this skip section");
        assert_eq!(
            err(&t("  - skip: { known_issues: [{ cluster_feature: a, fixed_by: b }] }\n")),
            "reason is mandatory within this skip section"
        );
        assert_eq!(err(&t("  - requires: { cluster_features: [f] }\n")), "reason is mandatory within this requires section");
        assert_eq!(
            err(&t("  - requires: { capabilities: [{ path: /x }] }\n")),
            "reason is mandatory within this requires section"
        );
        // Runner features, x-pack and awaits_fix need no reason; a reason in the other section
        // doesn't count.
        assert_eq!(ok(&t("  - requires: { test_runner_features: [headers] }\n")), Ok(()));
        assert_eq!(ok(&t("  - skip: { features: [xpack] }\n")), Ok(()));
        assert_eq!(ok(&t("  - skip: { awaits_fix: \"#1\" }\n")), Ok(()));
        assert_eq!(
            err(&t("  - requires: { cluster_features: [f], reason: r }\n  - skip: { cluster_features: [g] }\n")),
            "reason is mandatory within this skip section"
        );
        assert_eq!(ok(&t("  - requires: { cluster_features: [f], reason: r }\n  - skip: { cluster_features: [g], reason: s }\n")), Ok(()));
        // Sections without prerequisites are not validated.
        assert_eq!(ok(&t("")), Ok(()));
    }

    #[test]
    fn length_values_are_converted_when_parsing() {
        let s = parse("\"t\":\n  - length: { a: \"3\" }\n  - length: { b: 2.9 }\n  - length: { c: 4 }\n");
        let want = [3, 2, 4];
        for (step, n) in s.tests[0].steps.iter().zip(want) {
            let StepKind::Assert(a) = &step.kind else { panic!() };
            assert_eq!(a.expected, Value::Int(n));
        }
    }

    #[test]
    fn java_parser_quirks() {
        // Null entries are skipped; a second API in one `do` replaces the first.
        let s = parse("\"t\":\n  - do:\n      warnings:\n      indices.delete: { index: a }\n      search: { index: b, q: ~ }\n");
        let StepKind::Do(d) = &s.tests[0].steps[0].kind else { panic!() };
        assert_eq!(d.api, "search");
        assert_eq!(d.params, vec![("index".to_owned(), "b".to_owned())]);
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn ids_from_paths() {
        let (id, area) = suite_id(Path::new("/r/test/free"), Path::new("/r/test/free/search.vectors/10_x.yml"));
        assert_eq!((id.as_str(), area.as_str()), ("search.vectors/10_x", "search.vectors"));
        let (id, area) = suite_id(Path::new("/r"), Path::new("/r/a.yml"));
        assert_eq!((id.as_str(), area.as_str()), ("a", ""));
    }
}
