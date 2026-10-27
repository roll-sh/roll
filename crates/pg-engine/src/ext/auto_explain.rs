//! auto_explain: the plans of statements that ran longer than auto_explain.log_min_duration,
//! written to the server log (and to the client past client_min_messages) at
//! auto_explain.log_level, as explain_ExecutorEnd lays them out: "duration: X ms  plan:",
//! the query text, its parameters, then the plan in auto_explain.log_format.

use crate::error::Severity;
use crate::session::settings::Settings;
use crate::sql::explain::{Format, Options};

/// What explain_ExecutorStart decided for one statement.
pub struct Run {
    pub opts: Options,
    min_ms: i64,
    pub level: Severity,
    max_param: i64,
}

/// Whether the statement about to run (`nested`: inside another) is explained, and how.
pub fn start(st: &Settings, nested: bool) -> Option<Run> {
    if !st.module_loaded("auto_explain") {
        return None;
    }
    let int = |n: &str| st.get_str(n).and_then(|v| v.parse::<i64>().ok()).unwrap_or(-1);
    let on = |n: &str| st.get_str(n) == Some("on");
    let min_ms = int("auto_explain.log_min_duration");
    if min_ms < 0 || (nested && !on("auto_explain.log_nested_statements")) {
        return None;
    }
    let rate: f64 = st.get_str("auto_explain.sample_rate").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    if rate < 1.0 {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        if (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64 >= rate {
            return None;
        }
    }
    let mut opts = crate::sql::explain::options(&[]).ok()?;
    opts.analyze = on("auto_explain.log_analyze");
    opts.verbose = on("auto_explain.log_verbose");
    opts.buffers = on("auto_explain.log_buffers") && opts.analyze;
    opts.wal = on("auto_explain.log_wal") && opts.analyze;
    opts.timing = on("auto_explain.log_timing") && opts.analyze;
    // (es->summary is set, but ExplainPrintPlan prints no planning or execution time.)
    opts.summary = false;
    opts.settings = on("auto_explain.log_settings");
    opts.format = match st.get_str("auto_explain.log_format") {
        Some("xml") => Format::Xml,
        Some("json") => Format::Json,
        Some("yaml") => Format::Yaml,
        _ => Format::Text,
    };
    let level = match st.get_str("auto_explain.log_level") {
        Some("info") => Severity::Info,
        Some("notice") => Severity::Notice,
        Some("warning") => Severity::Warning,
        Some("log") | None => Severity::Log,
        _ => Severity::Debug,
    };
    Some(Run { opts, min_ms, level, max_param: int("auto_explain.log_parameter_max_length") })
}

impl Run {
    /// The message for a statement that took `ms` (None: quicker than log_min_duration):
    /// `plan` is EXPLAIN's output, `query` the source text, `params` its parameters' text.
    pub fn message(&self, ms: f64, plan: &str, query: &str, params: &[Option<String>]) -> Option<String> {
        if ms < self.min_ms as f64 {
            return None;
        }
        let shown_params = (!params.is_empty() && self.max_param != 0).then(|| {
            params
                .iter()
                .enumerate()
                .map(|(i, v)| match v {
                    None => format!("${} = NULL", i + 1),
                    Some(t) => format!(
                        "${} = '{}'",
                        i + 1,
                        crate::session::serverlog_truncate(t, self.max_param).replace('\'', "''")
                    ),
                })
                .collect::<Vec<_>>()
                .join(", ")
        });
        let body = match self.opts.format {
            Format::Text => {
                let mut s = format!("Query Text: {query}\n");
                if let Some(p) = &shown_params {
                    s.push_str(&format!("Query Parameters: {p}\n"));
                }
                s.push_str(plan.trim_end_matches('\n'));
                s
            }
            Format::Json => {
                // ExplainBeginOutput's "[" and "]" become "{" and "}" around the query text
                // and the plan (ExplainPrintPlan opens no group of its own).
                let lines: Vec<&str> = plan.trim_end().lines().collect();
                let inner = lines.get(2..lines.len().saturating_sub(2)).unwrap_or(&[]);
                let mut s = format!("{{\n  \"Query Text\": {},\n", json_str(query));
                if let Some(p) = &shown_params {
                    s.push_str(&format!("  \"Query Parameters\": {},\n", json_str(p)));
                }
                let body: Vec<&str> = inner.iter().map(|l| l.strip_prefix("  ").unwrap_or(l)).collect();
                s.push_str(&body.join("\n"));
                s.push_str("\n}");
                s
            }
            _ => plan.trim_end_matches('\n').to_string(),
        };
        Some(format!("duration: {ms:.3} ms  plan:\n{body}"))
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
