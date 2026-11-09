//! The automations' template language (docs/code/automations.md, "Templates"): `{{ path }}`
//! fills in a value from the run's context (`event.…`, `run.…`, `steps.<id>.…`,
//! `automation.…`, `owner`), optionally through pipes (`{{ event.data.title | truncate:80 }}`).
//! It is logic-less on purpose: no conditionals, loops, calls or code; a path only reads, and
//! every pipe is a pure, bounded function. What a template renders to is at most
//! [`limits::TEMPLATE`] characters, so a template can't blow up a prompt, a body or a log.
//!
//! - Values: a string as it is, numbers and booleans as JSON writes them, null or missing as
//!   nothing, objects and lists as compact JSON.
//! - Paths: dot-separated keys (letters, digits, `_`, `-`) and list indexes (`labels.0`).
//! - Literals: `{{ "{{" }}` writes `{{`.
//! - Pipes: `default:"x"` (when empty), `upper`, `lower`, `trim`, `truncate:N` (characters, with
//!   `…`), `json` (JSON text, also for strings), `url` (percent-encoded for a URL component),
//!   `join:", "` (a list), `first`, `last`, `length`, `lines:N` (the first N lines).
//! - `secrets.<name>` reads the automation's sealed secrets, and only where a step allows it
//!   (a webhook's header values): everywhere else it's an error, so a secret can't end up in a
//!   prompt, a body or a log.

use agent_proto::automation::limits;
use serde_json::Value;
use std::fmt::Write as _;

/// A parsed template.
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq)]
enum Part {
    Text(String),
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
struct Expr {
    head: Head,
    pipes: Vec<Pipe>,
}

#[derive(Clone, Debug, PartialEq)]
enum Head {
    Path(Vec<String>),
    Literal(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Pipe {
    Default(String),
    Upper,
    Lower,
    Trim,
    Truncate(usize),
    Json,
    Url,
    Join(String),
    First,
    Last,
    Length,
    Lines(usize),
}

/// Where `secrets.…` may be read.
#[derive(Clone, Copy)]
pub enum Secrets<'a> {
    Refused,
    /// Look a secret up by name (`None`: there's no such secret).
    Allowed(&'a dyn Fn(&str) -> Option<String>),
}

const SECRETS: &str = "secrets";
/// Pipes per expression, and characters of one expression.
const MAX_PIPES: usize = 8;
const MAX_EXPR: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TemplateError(pub String);

fn err<T>(m: impl Into<String>) -> Result<T, TemplateError> {
    Err(TemplateError(m.into()))
}

impl Template {
    pub fn parse(src: &str) -> Result<Template, TemplateError> {
        if src.chars().count() > limits::TEMPLATE {
            return err(format!("a template may be at most {} characters", limits::TEMPLATE));
        }
        let mut parts = Vec::new();
        let mut rest = src;
        while let Some(open) = rest.find("{{") {
            if open > 0 {
                parts.push(Part::Text(rest[..open].to_string()));
            }
            let after = &rest[open + 2..];
            let close = close_of(after).ok_or_else(|| TemplateError(format!("unclosed {{{{ at “{}”", snippet(&rest[open..]))))?;
            let inner = &after[..close];
            if inner.len() > MAX_EXPR {
                return err(format!("an expression may be at most {MAX_EXPR} characters"));
            }
            parts.push(Part::Expr(parse_expr(inner)?));
            rest = &after[close + 2..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_string()));
        }
        Ok(Template { parts })
    }

    /// Whether it has no expressions (renders to itself).
    pub fn is_plain(&self) -> bool {
        self.parts.iter().all(|p| matches!(p, Part::Text(_)))
    }

    /// The paths it reads (`["event", "data", "title"]`), for checks at save time.
    pub fn paths(&self) -> impl Iterator<Item = &[String]> {
        self.parts.iter().filter_map(|p| match p {
            Part::Expr(Expr { head: Head::Path(p), .. }) => Some(p.as_slice()),
            _ => None,
        })
    }

    /// Whether it reads `secrets.…`.
    pub fn reads_secrets(&self) -> bool {
        self.paths().any(|p| p.first().is_some_and(|h| h == SECRETS))
    }

    /// The template filled in from `ctx`.
    pub fn render(&self, ctx: &Value, secrets: Secrets<'_>) -> Result<String, TemplateError> {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Text(t) => out.push_str(t),
                Part::Expr(e) => out.push_str(&text_of(&eval(e, ctx, secrets)?)),
            }
            if out.chars().count() > limits::TEMPLATE {
                return err(format!("the template renders to more than {} characters", limits::TEMPLATE));
            }
        }
        Ok(out)
    }

    /// Like [`render`](Self::render), but a template that is exactly one expression keeps the
    /// value's JSON type (`"{{ event.data.change }}"` is the change object, not its text).
    pub fn render_value(&self, ctx: &Value, secrets: Secrets<'_>) -> Result<Value, TemplateError> {
        if let [Part::Expr(e)] = self.parts.as_slice() {
            let v = eval(e, ctx, secrets)?;
            if serde_json::to_string(&v).map_or(0, |s| s.len()) > limits::TEMPLATE {
                return err(format!("the template renders to more than {} characters", limits::TEMPLATE));
            }
            return Ok(v);
        }
        self.render(ctx, secrets).map(Value::String)
    }
}

/// Render a string as a template.
pub fn render(src: &str, ctx: &Value) -> Result<String, TemplateError> {
    Template::parse(src)?.render(ctx, Secrets::Refused)
}

/// Render every string in `v` (keys stay as they are); single-expression strings keep their
/// value's type.
pub fn render_json(v: &Value, ctx: &Value, secrets: Secrets<'_>) -> Result<Value, TemplateError> {
    let mut total = 0usize;
    let out = render_json_inner(v, ctx, secrets, 0)?;
    walk_size(&out, &mut total);
    if total > limits::OUTPUT * 4 {
        return err("the rendered config is too large");
    }
    Ok(out)
}

fn render_json_inner(v: &Value, ctx: &Value, secrets: Secrets<'_>, depth: usize) -> Result<Value, TemplateError> {
    if depth > 32 {
        return err("the config is nested too deep");
    }
    Ok(match v {
        Value::String(s) if s.contains("{{") => Template::parse(s)?.render_value(ctx, secrets)?,
        Value::Array(a) => Value::Array(a.iter().map(|x| render_json_inner(x, ctx, secrets, depth + 1)).collect::<Result<_, _>>()?),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| Ok((k.clone(), render_json_inner(x, ctx, secrets, depth + 1)?))).collect::<Result<_, TemplateError>>()?),
        other => other.clone(),
    })
}

fn walk_size(v: &Value, total: &mut usize) {
    match v {
        Value::String(s) => *total += s.len(),
        Value::Array(a) => a.iter().for_each(|x| walk_size(x, total)),
        Value::Object(o) => o.iter().for_each(|(k, x)| {
            *total += k.len();
            walk_size(x, total)
        }),
        _ => *total += 8,
    }
}

/// Every template in a JSON config parses (and none reads secrets unless `secrets_ok` says
/// that path may): the save-time check. `at` is where `v` is, for messages.
pub fn check_json(v: &Value, at: &str, secrets_ok: &dyn Fn(&str) -> bool) -> Result<(), TemplateError> {
    match v {
        Value::String(s) if s.contains("{{") => {
            let t = Template::parse(s).map_err(|e| TemplateError(format!("{at}: {e}")))?;
            if t.reads_secrets() && !secrets_ok(at) {
                return err(format!("{at}: secrets can only be used in a webhook step's header values"));
            }
            Ok(())
        }
        Value::Array(a) => a.iter().enumerate().try_for_each(|(i, x)| check_json(x, &format!("{at}.{i}"), secrets_ok)),
        Value::Object(o) => o.iter().try_for_each(|(k, x)| check_json(x, &format!("{at}.{k}"), secrets_ok)),
        _ => Ok(()),
    }
}

/// Where the `}}` closing an expression is, skipping quoted strings.
fn close_of(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut quoted = false;
    while i < b.len() {
        match b[i] {
            b'\\' if quoted => i += 1,
            b'"' => quoted = !quoted,
            b'}' if !quoted && b.get(i + 1) == Some(&b'}') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn snippet(s: &str) -> String {
    s.chars().take(24).collect()
}

/// Split on `|` outside quotes.
fn split_pipes(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted, mut esc) = (0, false, false);
    for (i, c) in s.char_indices() {
        match c {
            _ if esc => esc = false,
            '\\' if quoted => esc = true,
            '"' => quoted = !quoted,
            '|' if !quoted => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn parse_expr(inner: &str) -> Result<Expr, TemplateError> {
    let mut segs = split_pipes(inner).into_iter().map(str::trim);
    let head = segs.next().unwrap_or_default();
    if head.is_empty() {
        return err("an empty {{ }}");
    }
    let head = if head.starts_with('"') {
        Head::Literal(string_lit(head)?)
    } else {
        let path: Vec<String> = head.split('.').map(str::to_string).collect();
        if path.iter().any(|s| s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
            return err(format!("“{head}” isn't a path (dot-separated names and indexes)"));
        }
        Head::Path(path)
    };
    let pipes: Vec<Pipe> = segs.map(parse_pipe).collect::<Result<_, _>>()?;
    if pipes.len() > MAX_PIPES {
        return err(format!("at most {MAX_PIPES} pipes"));
    }
    Ok(Expr { head, pipes })
}

fn string_lit(s: &str) -> Result<String, TemplateError> {
    let inner = s.strip_prefix('"').and_then(|r| r.strip_suffix('"')).filter(|_| s.len() >= 2).ok_or_else(|| TemplateError(format!("unclosed string {s}")))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(c) => out.push(c),
                None => return err("a string ends in \\"),
            },
            '"' => return err(format!("a quote inside {s}: write \\\"")),
            c => out.push(c),
        }
    }
    Ok(out)
}

fn parse_pipe(s: &str) -> Result<Pipe, TemplateError> {
    let (name, arg) = match s.split_once(':') {
        Some((n, a)) => (n.trim(), Some(a.trim())),
        None => (s, None),
    };
    let num = |a: Option<&str>| -> Result<usize, TemplateError> {
        a.and_then(|a| a.parse::<usize>().ok()).filter(|n| *n > 0 && *n <= limits::TEMPLATE).ok_or_else(|| TemplateError(format!("{name} needs a number, as {name}:80")))
    };
    let text = |a: Option<&str>| -> Result<String, TemplateError> {
        match a {
            Some(a) if a.starts_with('"') => string_lit(a),
            _ => err(format!("{name} needs a string, as {name}:\"…\"")),
        }
    };
    Ok(match (name, arg) {
        ("default", a) => Pipe::Default(text(a)?),
        ("upper", None) => Pipe::Upper,
        ("lower", None) => Pipe::Lower,
        ("trim", None) => Pipe::Trim,
        ("truncate", a) => Pipe::Truncate(num(a)?),
        ("json", None) => Pipe::Json,
        ("url", None) => Pipe::Url,
        ("join", None) => Pipe::Join(", ".into()),
        ("join", a) => Pipe::Join(text(a)?),
        ("first", None) => Pipe::First,
        ("last", None) => Pipe::Last,
        ("length", None) => Pipe::Length,
        ("lines", a) => Pipe::Lines(num(a)?),
        (n @ ("upper" | "lower" | "trim" | "json" | "url" | "first" | "last" | "length"), Some(_)) => return err(format!("{n} takes no argument")),
        (other, _) => return err(format!("no pipe “{other}” (default, upper, lower, trim, truncate, json, url, join, first, last, length, lines)")),
    })
}

fn lookup<'a>(ctx: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut v = ctx;
    for seg in path {
        v = match v {
            Value::Object(o) => o.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

fn eval(e: &Expr, ctx: &Value, secrets: Secrets<'_>) -> Result<Value, TemplateError> {
    let mut v = match &e.head {
        Head::Literal(s) => Value::String(s.clone()),
        Head::Path(p) if p[0] == SECRETS => match secrets {
            Secrets::Refused => return err("secrets can only be used in a webhook step's header values"),
            Secrets::Allowed(get) => match p.as_slice() {
                [_, name] => Value::String(get(name).ok_or_else(|| TemplateError(format!("the automation has no secret {name}")))?),
                _ => return err("a secret is {{ secrets.<name> }}"),
            },
        },
        Head::Path(p) => lookup(ctx, p).cloned().unwrap_or(Value::Null),
    };
    for p in &e.pipes {
        v = apply(p, v);
    }
    Ok(v)
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

fn apply(p: &Pipe, v: Value) -> Value {
    let s = |f: &dyn Fn(String) -> String| Value::String(f(text_of(&v)));
    match p {
        Pipe::Default(d) if is_empty(&v) => Value::String(d.clone()),
        Pipe::Default(_) => v,
        Pipe::Upper => s(&|t| t.to_uppercase()),
        Pipe::Lower => s(&|t| t.to_lowercase()),
        Pipe::Trim => s(&|t| t.trim().to_string()),
        Pipe::Truncate(n) => s(&|t| {
            if t.chars().count() <= *n {
                t
            } else {
                let mut out: String = t.chars().take(n.saturating_sub(1)).collect();
                out.push('…');
                out
            }
        }),
        Pipe::Json => Value::String(v.to_string()),
        Pipe::Url => s(&|t| percent_encoding::utf8_percent_encode(&t, percent_encoding::NON_ALPHANUMERIC).to_string()),
        Pipe::Join(sep) => match &v {
            Value::Array(a) => {
                let mut out = String::new();
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(sep);
                    }
                    let _ = write!(out, "{}", label_text(x));
                    if out.len() > limits::TEMPLATE {
                        break;
                    }
                }
                Value::String(out)
            }
            _ => v,
        },
        Pipe::First => match v {
            Value::Array(a) => a.into_iter().next().unwrap_or(Value::Null),
            other => other,
        },
        Pipe::Last => match v {
            Value::Array(a) => a.into_iter().last().unwrap_or(Value::Null),
            other => other,
        },
        Pipe::Length => Value::from(match &v {
            Value::Array(a) => a.len(),
            Value::Object(o) => o.len(),
            Value::Null => 0,
            other => text_of(other).chars().count(),
        }),
        Pipe::Lines(n) => s(&|t| t.lines().take(*n).collect::<Vec<_>>().join("\n")),
    }
}

/// A list item as text: a label object by its name.
fn label_text(v: &Value) -> String {
    match v.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => text_of(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Value {
        json!({
            "event": { "data": { "title": "Fix the queue", "n": 12, "ok": true, "labels": [{ "name": "bug" }, "p1"], "change": { "url": "https://x/1" }, "text": "a\nb\nc" } },
            "steps": { "start": { "thread": "thr_1" } },
            "owner": "acme",
        })
    }

    fn r(s: &str) -> String {
        render(s, &ctx()).unwrap()
    }

    #[test]
    fn paths_fill_in() {
        assert_eq!(r("{{event.data.title}} by {{ owner }}"), "Fix the queue by acme");
        assert_eq!(r("#{{event.data.n}} {{event.data.ok}}"), "#12 true");
        assert_eq!(r("{{event.data.labels.1}}"), "p1");
        assert_eq!(r("[{{event.data.missing}}]"), "[]");
        assert_eq!(r("{{event.data.change}}"), r#"{"url":"https://x/1"}"#);
        assert_eq!(r("{{steps.start.thread}}"), "thr_1");
        assert_eq!(r("no braces"), "no braces");
        assert_eq!(r(r#"{{ "{{" }}x}}"#), "{{x}}");
    }

    #[test]
    fn pipes() {
        assert_eq!(r("{{event.data.title | upper}}"), "FIX THE QUEUE");
        assert_eq!(r("{{event.data.title|lower|truncate:5}}"), "fix …");
        assert_eq!(r(r#"{{event.data.none | default:"nobody"}}"#), "nobody");
        assert_eq!(r("{{event.data.labels | join}}"), "bug, p1");
        assert_eq!(r(r#"{{event.data.labels | join:" / "}}"#), "bug / p1");
        assert_eq!(r("{{event.data.labels | length}} {{event.data.title|length}}"), "2 13");
        assert_eq!(r("{{event.data.labels | first | json}}"), r#"{"name":"bug"}"#);
        assert_eq!(r("{{event.data.title | json}}"), r#""Fix the queue""#);
        assert_eq!(r("{{event.data.title | url}}"), "Fix%20the%20queue");
        assert_eq!(r("{{event.data.text | lines:2}}"), "a\nb");
        assert_eq!(r(r#"{{ "  x " | trim }}"#), "x");
    }

    #[test]
    fn single_expressions_keep_their_type() {
        let t = Template::parse("{{event.data.change}}").unwrap();
        assert_eq!(t.render_value(&ctx(), Secrets::Refused).unwrap(), json!({ "url": "https://x/1" }));
        let v = render_json(&json!({ "n": "{{event.data.n}}", "s": "n={{event.data.n}}", "k": [1, "{{owner}}"] }), &ctx(), Secrets::Refused).unwrap();
        assert_eq!(v, json!({ "n": 12, "s": "n=12", "k": [1, "acme"] }));
    }

    #[test]
    fn bad_templates_are_refused() {
        for bad in ["{{", "{{ }}", "{{a b}}", "{{a | nope}}", "{{a | truncate}}", "{{a | truncate:x}}", r#"{{"open}}"#, "{{a|upper:1}}", "{{a..b}}"] {
            assert!(Template::parse(bad).is_err(), "{bad}");
        }
        let many = format!("{{{{a{}}}}}", "|upper".repeat(MAX_PIPES + 1));
        assert!(Template::parse(&many).is_err());
    }

    #[test]
    fn output_is_bounded() {
        let big = "x".repeat(40_000);
        let c = json!({ "big": big });
        assert!(render("{{big}}{{big}}", &c).is_err());
        assert!(render("{{big}}", &c).is_ok());
        assert!(Template::parse(&"y".repeat(limits::TEMPLATE + 1)).is_err());
    }

    #[test]
    fn secrets_only_where_allowed() {
        let t = Template::parse("Bearer {{secrets.TOKEN}}").unwrap();
        assert!(t.reads_secrets());
        assert!(t.render(&ctx(), Secrets::Refused).is_err());
        let get = |n: &str| (n == "TOKEN").then(|| "s3cret".to_string());
        assert_eq!(t.render(&ctx(), Secrets::Allowed(&get)).unwrap(), "Bearer s3cret");
        assert!(Template::parse("{{secrets.OTHER}}").unwrap().render(&ctx(), Secrets::Allowed(&get)).is_err());
        let cfg = json!({ "headers": { "a": "{{secrets.TOKEN}}" }, "body": "{{secrets.TOKEN}}" });
        let headers_only = |at: &str| at.starts_with("config.headers.");
        let e = check_json(&cfg, "config", &headers_only).unwrap_err();
        assert!(e.0.contains("config.body"), "{e}");
        assert!(check_json(&json!({ "headers": { "a": "{{secrets.TOKEN}}" } }), "config", &headers_only).is_ok());
    }
}
