//! Formatting helpers for `roll queues`: aligned tables, counts, rates, sparklines.

use serde_json::Value;

/// A JSON number as f64 (missing or non-numeric → 0).
pub fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// 1234567 → "1,234,567" (rounded).
pub fn count(n: f64) -> String {
    let n = n.round();
    let digits = format!("{}", n.abs() as u64);
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0.0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A message rate: "0", "2.5", "1,234".
pub fn rate(r: f64) -> String {
    if r < 0.05 {
        "0".into()
    } else if r < 10.0 {
        format!("{r:.1}")
    } else {
        count(r)
    }
}

/// Dollars: two decimals, four below a cent so tiny bills don't read as $0.00.
pub fn usd(x: f64) -> String {
    if x != 0.0 && x.abs() < 0.01 {
        format!("${x:.4}")
    } else {
        format!("${x:.2}")
    }
}

/// A unit price: up to four decimals, at least two, trailing zeros dropped ($0.023, $0.40).
pub fn price(x: f64) -> String {
    let mut s = format!("{x:.4}");
    while s.ends_with('0') && s.len() - s.find('.').unwrap_or(0) > 3 {
        s.pop();
    }
    format!("${s}")
}

/// Bytes, binary units: "512 B", "1.5 KB", "3.3 MB".
pub fn size(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut n, mut i) = (n.max(0.0), 0);
    while n >= 1024.0 && i < UNITS.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n:.0} B")
    } else {
        format!("{n:.1} {}", UNITS[i])
    }
}

/// A duration in seconds, two units at most: "45s", "12m 3s", "4h 12m", "3d 4h".
pub fn span(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        3600..86_400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        _ => format!("{}d {}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// Unix seconds → "YYYY-MM-DD" (UTC).
pub fn date(ts: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// One block character per sample, scaled to the largest sample.
pub fn spark(xs: &[f64]) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = xs.iter().copied().fold(0.0, f64::max);
    xs.iter()
        .map(|&x| {
            if max <= 0.0 {
                BLOCKS[0]
            } else {
                BLOCKS[((x.max(0.0) / max) * 7.0).round() as usize]
            }
        })
        .collect()
}

/// A progress bar `width` cells wide.
pub fn bar(frac: f64, width: usize) -> String {
    let filled = ((frac.clamp(0.0, 1.0)) * width as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Cut `s` to at most `max` characters, marking the cut with "…".
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Compact JSON for argument maps; empty or null → "-".
pub fn compact(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::Object(m) if m.is_empty() => "-".into(),
        Value::Object(m) => m
            .iter()
            .map(|(k, v)| format!("{k}={}", scalar(v)))
            .collect::<Vec<_>>()
            .join(" "),
        v => scalar(v),
    }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// The default exchange has an empty name; show it the way RabbitMQ does.
pub fn exchange_name(name: &str) -> &str {
    if name.is_empty() { "(default)" } else { name }
}

/// `--filter`: a glob when it has `*` or `?`, otherwise a substring.
pub fn matches(filter: Option<&str>, name: &str) -> bool {
    match filter {
        None | Some("") => true,
        Some(p) if p.contains(['*', '?']) => glob(p.as_bytes(), name.as_bytes()),
        Some(p) => name.contains(p),
    }
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    // Iterative wildcard match with single-star backtracking.
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    L,
    R,
}

/// A plain, two-space separated table with a lowercase header row.
pub struct Table {
    cols: Vec<(&'static str, Align)>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(cols: &[(&'static str, Align)]) -> Self {
        Table {
            cols: cols.to_vec(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn render(&self) -> String {
        let n = self.cols.len();
        let mut w: Vec<usize> = self.cols.iter().map(|(h, _)| h.chars().count()).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                w[i] = w[i].max(c.chars().count());
            }
        }
        let line = |cells: &mut dyn Iterator<Item = &str>| {
            let mut out = String::new();
            for (i, c) in cells.enumerate().take(n) {
                if i > 0 {
                    out.push_str("  ");
                }
                let pad = " ".repeat(w[i] - c.chars().count());
                match self.cols[i].1 {
                    Align::R => {
                        out.push_str(&pad);
                        out.push_str(c);
                    }
                    Align::L => {
                        out.push_str(c);
                        out.push_str(&pad);
                    }
                }
            }
            out.truncate(out.trim_end().len());
            out.push('\n');
            out
        };
        // A table whose headers are all empty is a plain grid.
        let mut out = if self.cols.iter().all(|(h, _)| h.is_empty()) {
            String::new()
        } else {
            line(&mut self.cols.iter().map(|(h, _)| *h))
        };
        for r in &self.rows {
            out.push_str(&line(&mut r.iter().map(String::as_str)));
        }
        out
    }

    pub fn print(&self) {
        print!("{}", self.render());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts() {
        assert_eq!(count(0.0), "0");
        assert_eq!(count(999.0), "999");
        assert_eq!(count(1000.0), "1,000");
        assert_eq!(count(1234567.4), "1,234,567");
        assert_eq!(count(-12345.0), "-12,345");
    }

    #[test]
    fn rates() {
        assert_eq!(rate(0.0), "0");
        assert_eq!(rate(0.01), "0");
        assert_eq!(rate(2.54), "2.5");
        assert_eq!(rate(12345.6), "12,346");
    }

    #[test]
    fn money() {
        assert_eq!(usd(0.0), "$0.00");
        assert_eq!(usd(0.00123), "$0.0012");
        assert_eq!(usd(12.345), "$12.35");
    }

    #[test]
    fn prices_and_sizes() {
        assert_eq!(price(0.023), "$0.023");
        assert_eq!(price(0.4), "$0.40");
        assert_eq!(price(0.0005), "$0.0005");
        assert_eq!(price(12.0), "$12.00");
        assert_eq!(size(0.0), "0 B");
        assert_eq!(size(512.0), "512 B");
        assert_eq!(size(1536.0), "1.5 KB");
        assert_eq!(size(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 GB");
    }

    #[test]
    fn spans_and_dates() {
        assert_eq!(span(45), "45s");
        assert_eq!(span(723), "12m 3s");
        assert_eq!(span(4 * 3600 + 12 * 60), "4h 12m");
        assert_eq!(span(3 * 86_400 + 4 * 3600 + 5), "3d 4h");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_700_000_000), "2023-11-14");
        assert_eq!(date(951_782_400), "2000-02-29");
    }

    #[test]
    fn sparkline_and_bar() {
        assert_eq!(spark(&[0.0, 0.0]), "▁▁");
        assert_eq!(spark(&[0.0, 5.0, 10.0]), "▁▅█");
        assert_eq!(spark(&[]), "");
        assert_eq!(bar(0.5, 4), "██░░");
        assert_eq!(bar(2.0, 3), "███");
        assert_eq!(bar(-1.0, 2), "░░");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("héllo", 3), "hé…");
    }

    #[test]
    fn compact_args() {
        assert_eq!(compact(&json!({})), "-");
        assert_eq!(compact(&Value::Null), "-");
        assert_eq!(
            compact(&json!({"x-max-length": 10, "x-queue-mode": "lazy"})),
            "x-max-length=10 x-queue-mode=lazy"
        );
    }

    #[test]
    fn filters() {
        assert!(matches(None, "orders"));
        assert!(matches(Some("der"), "orders"));
        assert!(!matches(Some("pay"), "orders"));
        assert!(matches(Some("orders.*"), "orders.eu"));
        assert!(!matches(Some("orders.*"), "orders"));
        assert!(matches(Some("*.dlq"), "orders.dlq"));
        assert!(matches(Some("o?ders*"), "orders.eu.dlq"));
        assert!(!matches(Some("*.dlq"), "orders.dlq.x"));
        assert!(matches(Some("*"), ""));
    }

    #[test]
    fn table_aligns_numbers_right_and_trims() {
        let mut t = Table::new(&[("name", Align::L), ("ready", Align::R), ("note", Align::L)]);
        t.row(vec!["orders".into(), "1,234".into(), "".into()]);
        t.row(vec!["q".into(), "5".into(), "dlq".into()]);
        assert_eq!(
            t.render(),
            "name    ready  note\norders  1,234\nq           5  dlq\n"
        );

        let mut grid = Table::new(&[("", Align::L), ("", Align::R)]);
        grid.row(vec!["queues".into(), "3".into()]);
        grid.row(vec!["ready".into(), "123,456".into()]);
        assert_eq!(grid.render(), "queues        3\nready   123,456\n");
    }
}
