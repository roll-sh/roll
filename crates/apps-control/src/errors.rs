//! An environment's errors, grouped (Cloud Run's Error Reporting, Sentry's issues, from the log
//! alone; docs/apps/README.md, "Errors"). Read from its log when asked: each instance's stack
//! traces (Python's `Traceback`, a JVM's `Exception … at`, Node's `Error … at`, Go's `panic:`)
//! are joined from their lines, a trace's fingerprint is its error's type and first frames with
//! line numbers, addresses and ids taken out, and an error line with no trace is grouped by its
//! message the same way. Each group: how many, first and last seen, on how many instances, and
//! one whole sample.

use crate::logs::{Level, LogLine, Stream};
use crate::{Apps, Result};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// Lines one read looks at, groups an answer lists, frames a fingerprint takes, and a sample's size.
const MAX_LINES: usize = 100_000;
const MAX_GROUPS: usize = 50;
const FRAMES: usize = 3;
const MAX_SAMPLE: usize = 8 << 10;
/// The most lines one trace joins.
const MAX_TRACE_LINES: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Python,
    Java,
    Node,
    Go,
    /// An error line with no trace.
    Line,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Python => "python",
            Kind::Java => "java",
            Kind::Node => "node",
            Kind::Go => "go",
            Kind::Line => "line",
        }
    }
}

/// A qualified class name: `java.lang.IllegalStateException`, `com.acme.Oops$Inner`.
fn java_class(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')) && (s.ends_with("Exception") || s.ends_with("Error") || s.ends_with("Throwable"))
}

/// A JavaScript error's first line: `TypeError: x`, `Error`, `Uncaught RangeError: x`.
fn js_error(s: &str) -> bool {
    let s = s.strip_prefix("Uncaught ").unwrap_or(s);
    let name = s.split(':').next().unwrap_or("");
    !name.is_empty() && name.ends_with("Error") && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The trace a line starts, if it starts one.
fn starts(text: &str) -> Option<Kind> {
    let t = text.trim_end();
    if t.starts_with("Traceback (most recent call last):") {
        return Some(Kind::Python);
    }
    if t.starts_with("panic: ") || t.starts_with("fatal error: ") {
        return Some(Kind::Go);
    }
    if let Some(rest) = t.strip_prefix("Exception in thread ") {
        return rest.split_whitespace().nth(1).is_some().then_some(Kind::Java);
    }
    let head = t.split(':').next().unwrap_or("");
    if java_class(head) {
        return Some(Kind::Java);
    }
    js_error(t).then_some(Kind::Node)
}

/// Whether a line goes on the trace being read (Python's last line, its error, ends it: `Some(true)`).
fn continues(kind: Kind, text: &str, lines: usize) -> Option<bool> {
    let t = text.trim_end();
    match kind {
        Kind::Python if t.starts_with(' ') || t.starts_with('\t') => Some(false),
        Kind::Python if lines > 1 && !t.is_empty() && t.split(':').next().is_some_and(|n| !n.contains(' ')) => Some(true),
        Kind::Python => None,
        Kind::Java if t.starts_with("\tat ") || t.starts_with("    at ") || t.starts_with("Caused by: ") || t.trim_start().starts_with("... ") || t.trim_start().starts_with("Suppressed: ") => Some(false),
        Kind::Node if t.starts_with("    at ") || t.starts_with("  at ") => Some(false),
        // Go: a blank after the panic, `goroutine N [running]:`, then function lines and their
        // tab-indented files.
        Kind::Go if t.is_empty() && lines == 1 => Some(false),
        Kind::Go if t.starts_with("goroutine ") || t.starts_with('\t') || t.starts_with("created by ") || (t.contains('(') && t.ends_with(')')) || t.starts_with("[signal ") => Some(false),
        _ => None,
    }
}

/// A line or frame with what changes between occurrences taken out: numbers, `0x…`, long hex
/// ids, quoted values.
fn normal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            out.push(c);
            for d in chars.by_ref() {
                if d == c {
                    break;
                }
            }
            out.push('…');
            out.push(c);
        } else if c.is_ascii_hexdigit() {
            let mut run = String::from(c);
            while let Some(&d) = chars.peek() {
                if d.is_ascii_alphanumeric() {
                    run.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            let numberish = run.chars().all(|x| x.is_ascii_digit()) || run.starts_with("0x") || (run.len() >= 8 && run.chars().all(|x| x.is_ascii_hexdigit()));
            out.push_str(if numberish { "0" } else { &run });
        } else {
            out.push(c);
        }
    }
    out
}

/// A trace's error (its type, and its message for a line or a panic) and its frames.
fn parts(kind: Kind, lines: &[&str]) -> (String, Vec<String>) {
    let frame = |l: &str| {
        let t = l.trim();
        t.starts_with("at ") || t.starts_with("File \"") || (kind == Kind::Go && l.starts_with('\t'))
    };
    let frames: Vec<String> = lines.iter().filter(|l| frame(l)).take(FRAMES).map(|l| normal(l.trim())).collect();
    let error = match kind {
        // The last line: `ValueError: bad input`.
        Kind::Python => lines.last().map(|l| l.split(':').next().unwrap_or(l).trim().to_string()).unwrap_or_default(),
        Kind::Go => normal(lines.first().copied().unwrap_or("")),
        Kind::Java => lines.first().map(|l| l.strip_prefix("Exception in thread ").and_then(|r| r.split_once(' ').map(|(_, c)| c)).unwrap_or(l).split(':').next().unwrap_or("").trim().to_string()).unwrap_or_default(),
        _ => lines.first().map(|l| l.trim_start_matches("Uncaught ").split(':').next().unwrap_or("").trim().to_string()).unwrap_or_default(),
    };
    (error, frames)
}

/// What an error line says: a JSON line's message (`msg`, `message`, `error`), else its text.
fn message(text: &str) -> String {
    if text.trim_start().starts_with('{')
        && let Ok(v) = serde_json::from_str::<Value>(text)
        && let Some(m) = ["msg", "message", "error", "err"].iter().find_map(|k| v[*k].as_str())
    {
        return m.to_string();
    }
    text.to_string()
}

/// One error as read: its kind, fingerprint, title, whole text, when and on which instance.
#[derive(Debug, PartialEq)]
pub struct Occurrence {
    pub kind: Kind,
    pub fingerprint: String,
    pub title: String,
    pub text: String,
    pub at: i64,
    pub instance: Option<String>,
}

fn occurrence(kind: Kind, lines: &[&str], at: i64, instance: Option<String>) -> Occurrence {
    use sha2::{Digest, Sha256};
    let (key, title) = if kind == Kind::Line {
        let m = message(lines[0]);
        (normal(m.trim()).chars().take(300).collect::<String>(), m.chars().take(300).collect())
    } else {
        let (error, frames) = parts(kind, lines);
        let title = match kind {
            Kind::Python => lines.last().copied().unwrap_or("").to_string(),
            _ => lines.first().copied().unwrap_or("").to_string(),
        };
        (format!("{}\n{error}\n{}", kind.name(), frames.join("\n")), title.chars().take(300).collect())
    };
    let fingerprint = Sha256::digest(key.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
    let mut text = lines.join("\n");
    if text.len() > MAX_SAMPLE {
        let mut cut = MAX_SAMPLE;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Occurrence { kind, fingerprint, title, text, at, instance }
}

/// The errors in an environment's lines (oldest first): each instance's and stream's traces
/// joined, and error lines with no trace on their own.
pub fn occurrences(lines: &[LogLine]) -> Vec<Occurrence> {
    struct Open {
        kind: Kind,
        lines: Vec<usize>,
    }
    let mut out = Vec::new();
    let mut open: HashMap<(Option<String>, u8), Open> = HashMap::new();
    let close = |o: Open, out: &mut Vec<Occurrence>| {
        let first = &lines[o.lines[0]];
        let texts: Vec<&str> = o.lines.iter().map(|i| lines[*i].text.as_str()).collect();
        out.push(occurrence(o.kind, &texts, first.t, first.instance.clone()));
    };
    for (i, l) in lines.iter().enumerate() {
        if !matches!(l.stream, Stream::Stdout | Stream::Stderr) {
            continue;
        }
        let key = (l.instance.clone(), l.stream as u8);
        if let Some(o) = open.get_mut(&key) {
            match continues(o.kind, &l.text, o.lines.len()) {
                Some(end) if o.lines.len() < MAX_TRACE_LINES => {
                    o.lines.push(i);
                    if end {
                        let o = open.remove(&key).expect("open");
                        close(o, &mut out);
                    }
                    continue;
                }
                _ => {
                    let o = open.remove(&key).expect("open");
                    close(o, &mut out);
                }
            }
        }
        if let Some(kind) = starts(&l.text) {
            open.insert(key, Open { kind, lines: vec![i] });
        } else if l.level.is_some_and(|lv| lv >= Level::Error) {
            out.push(occurrence(Kind::Line, &[l.text.as_str()], l.t, l.instance.clone()));
        }
    }
    let mut rest: Vec<Open> = open.into_values().collect();
    rest.sort_by_key(|o| o.lines[0]);
    for o in rest {
        close(o, &mut out);
    }
    // In the order they began (a trace still open at the end closes last).
    out.sort_by_key(|o| o.at);
    out
}

/// What an error is, never what it says (a notification carries this, not a line of the log): its
/// type (`ZeroDivisionError`, `java.lang.IllegalStateException`), `panic`, or `an error line`.
pub fn name_of(o: &Occurrence) -> String {
    let lines: Vec<&str> = o.text.lines().collect();
    let name = match o.kind {
        Kind::Go => "panic".to_string(),
        Kind::Line => return "an error line".into(),
        k => parts(k, &lines).0,
    };
    let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || "._$".contains(*c)).take(120).collect();
    if name.is_empty() { "an error".into() } else { name }
}

impl Apps {
    /// New errors in an environment's log since it was last looked at (the alerts' pass, each
    /// minute on the owner): a fingerprint it has never had is an `error.new` event, once ever
    /// (kept in `error_groups`, so a restart doesn't say it again). The event names the error's
    /// type, never its message.
    pub async fn note_new_errors(self: &Arc<Self>, l: &crate::live::EnvLive) {
        let key = crate::env_key(&l.service, &l.env);
        let now = code_sql::now_ms();
        let since = self.errors_seen.lock().insert(key, now).unwrap_or(now - 60_000);
        // A little before, so a trace that began just before the last look is read whole.
        let (lines, _) = self.logs.read(&format!("env:{}/{}", l.service, l.env), |x| matches!(x.stream, Stream::Stdout | Stream::Stderr), Some(since - 30_000), None, 20_000).await;
        let mut said = BTreeSet::new();
        for o in occurrences(&lines).into_iter().filter(|o| o.at >= since) {
            if !said.insert(o.fingerprint.clone()) {
                continue;
            }
            let name = name_of(&o);
            let (sid, e, f, n, at) = (l.service.clone(), l.env.clone(), o.fingerprint.clone(), name.clone(), o.at);
            if !self.db(move |s| s.remember_error(&sid, &e, &f, &n, at)).await.unwrap_or(false) {
                continue;
            }
            let (sid, e, dep) = (l.service.clone(), l.env.clone(), l.template().map(|t| t.deployment.clone()));
            let data = json!({ "fingerprint": o.fingerprint, "kind": o.kind.name(), "name": name });
            let _ = self.db(move |s| s.add_event("error.new", &sid, &e, dep.as_deref(), &data)).await;
            l.log.stream(Stream::System).line(format!("a new error: {name} ({})", o.fingerprint));
        }
    }

    /// `GET …/environments/{env}/errors?minutes=`: the last 5–10,080 minutes' errors, grouped,
    /// the most frequent first (at most 50).
    pub async fn errors(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(5, 10_080);
        let since = code_sql::now_ms() - minutes * 60_000;
        // Oldest first, as a trace was written.
        let (lines, cut) = self.logs.read(&format!("env:{service}/{env}"), |l| matches!(l.stream, Stream::Stdout | Stream::Stderr), Some(since), None, MAX_LINES).await;
        let scanned = lines.len();
        struct Group {
            kind: Kind,
            title: String,
            count: usize,
            first: i64,
            last: i64,
            instances: BTreeSet<String>,
            sample: String,
        }
        let mut groups: HashMap<String, Group> = HashMap::new();
        for o in occurrences(&lines) {
            let g = groups.entry(o.fingerprint.clone()).or_insert_with(|| Group { kind: o.kind, title: o.title.clone(), count: 0, first: o.at, last: o.at, instances: BTreeSet::new(), sample: String::new() });
            g.count += 1;
            g.first = g.first.min(o.at);
            // The newest one is the sample, and its title the group's.
            if o.at >= g.last {
                g.last = o.at;
                g.sample = o.text;
                g.title = o.title;
            }
            if let Some(i) = o.instance {
                g.instances.insert(i);
            }
        }
        let mut list: Vec<(String, Group)> = groups.into_iter().collect();
        list.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(b.1.last.cmp(&a.1.last)));
        list.truncate(MAX_GROUPS);
        let groups: Vec<Value> = list
            .into_iter()
            .map(|(f, g)| json!({ "fingerprint": f, "kind": g.kind.name(), "title": g.title, "count": g.count, "first_seen": g.first, "last_seen": g.last, "instances": g.instances.len(), "sample": g.sample }))
            .collect();
        Ok(json!({ "minutes": minutes, "groups": groups, "scanned": scanned, "cut": cut || scanned >= MAX_LINES }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(texts: &[(&str, &str)]) -> Vec<LogLine> {
        texts
            .iter()
            .enumerate()
            .map(|(i, (inst, t))| LogLine { t: i as i64, stream: Stream::Stderr, instance: Some(inst.to_string()), deployment: None, text: t.to_string(), level: crate::logs::level_of(t) })
            .collect()
    }

    #[test]
    fn traces_join_across_their_lines_and_group_by_their_error_and_frames() {
        let py = |n: u32| {
            vec![
                ("a", "Traceback (most recent call last):".to_string()),
                ("a", format!("  File \"/app/main.py\", line {n}, in handler")),
                ("a", "    total = price / qty".to_string()),
                ("a", "ZeroDivisionError: division by zero".to_string()),
            ]
        };
        let mut all: Vec<(String, String)> = Vec::new();
        for (i, t) in py(10) {
            all.push((i.into(), t));
        }
        // Another instance's lines in between don't break it.
        all.push(("b".into(), "listening on :8080".into()));
        all.extend([("b", "TypeError: Cannot read properties of undefined (reading 'id')"), ("b", "    at handler (/app/server.js:12:18)"), ("b", "    at process (node:internal/x:5:3)")].map(|(i, t)| (i.to_string(), t.to_string())));
        for (i, t) in py(42) {
            all.push((i.into(), t));
        }
        all.extend([("c", "Exception in thread \"main\" java.lang.IllegalStateException: closed"), ("c", "\tat com.acme.Db.query(Db.java:88)"), ("c", "\tat com.acme.App.main(App.java:12)"), ("c", "Caused by: java.io.IOException: reset"), ("c", "\t... 2 more")].map(|(i, t)| (i.to_string(), t.to_string())));
        all.extend([("d", "panic: runtime error: index out of range [5] with length 3"), ("d", ""), ("d", "goroutine 1 [running]:"), ("d", "main.main()"), ("d", "\t/app/main.go:9 +0x1d")].map(|(i, t)| (i.to_string(), t.to_string())));
        all.extend([("e", r#"{"level":"error","msg":"order 1234 failed: card declined"}"#), ("e", r#"{"level":"error","msg":"order 98765 failed: card declined"}"#), ("e", "INFO all good")].map(|(i, t)| (i.to_string(), t.to_string())));
        let ls = lines(&all.iter().map(|(i, t)| (i.as_str(), t.as_str())).collect::<Vec<_>>());
        let found = occurrences(&ls);
        let kinds: Vec<Kind> = found.iter().map(|o| o.kind).collect();
        assert_eq!(kinds, [Kind::Python, Kind::Node, Kind::Python, Kind::Java, Kind::Go, Kind::Line, Kind::Line], "{found:#?}");
        // The same error at another line number is one group; its title is its error.
        assert_eq!(found[0].fingerprint, found[2].fingerprint);
        assert_eq!(found[0].title, "ZeroDivisionError: division by zero");
        assert_eq!(found[0].text.lines().count(), 4);
        assert_eq!(found[3].text.lines().count(), 5, "a cause and its tail stay on the trace");
        assert_eq!(found[5].fingerprint, found[6].fingerprint, "messages that differ by an id");
        assert_ne!(found[0].fingerprint, found[1].fingerprint);
    }
}
