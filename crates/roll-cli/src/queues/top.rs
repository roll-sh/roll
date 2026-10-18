//! `roll queues top`: rates and the deepest queues, redrawn in place until Ctrl-C.

use super::all_queues;
use super::fmt::{self, Align::L, Align::R, Table, count, f, rate};
use crate::{Api, s};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{IsTerminal, Write};
use std::time::Duration;

const HISTORY: usize = 48;

#[derive(Default)]
struct History {
    publish: VecDeque<f64>,
    deliver: VecDeque<f64>,
    ack: VecDeque<f64>,
}

impl History {
    fn push(&mut self, rates: &Value) {
        for (buf, k) in [
            (&mut self.publish, "publish"),
            (&mut self.deliver, "deliver"),
            (&mut self.ack, "ack"),
        ] {
            if buf.len() == HISTORY {
                buf.pop_front();
            }
            buf.push_back(f(&rates[k]));
        }
    }
}

pub async fn run(
    api: &Api,
    vhost: Option<&str>,
    interval: f64,
    limit: usize,
) -> anyhow::Result<()> {
    let every = Duration::from_secs_f64(interval.max(0.5));
    let tty = std::io::stdout().is_terminal();
    let mut hist = History::default();
    if !tty {
        // Piped: one snapshot, no escape codes.
        print!("{}", frame(api, vhost, limit, every, &mut hist).await);
        return Ok(());
    }
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Alternate screen, hidden cursor; both restored on the way out.
    print!("\x1b[?1049h\x1b[?25l");
    loop {
        let text = tokio::select! {
            _ = &mut ctrl_c => break,
            t = frame(api, vhost, limit, every, &mut hist) => t,
        };
        let mut out = std::io::stdout().lock();
        let _ = write!(out, "\x1b[H\x1b[2J{text}");
        let _ = out.flush();
        drop(out);
        tokio::select! {
            _ = &mut ctrl_c => break,
            _ = tokio::time::sleep(every) => {}
        }
    }
    print!("\x1b[?25h\x1b[?1049l");
    std::io::stdout().flush()?;
    Ok(())
}

async fn frame(
    api: &Api,
    vhost: Option<&str>,
    limit: usize,
    every: Duration,
    hist: &mut History,
) -> String {
    let refresh = format!("every {}s · ctrl-c to quit", every.as_secs_f64());
    let o = match api.get("/v1/queues/overview").await {
        Ok(o) => o,
        Err(e) => return format!("roll queues  ·  {refresh}\n\n  {e:#}\n  retrying…\n"),
    };
    hist.push(&o["rates"]);
    let queues = all_queues(api, vhost).await;
    render(&o, queues, hist, vhost, limit, &refresh)
}

fn render(
    o: &Value,
    queues: anyhow::Result<Vec<Value>>,
    hist: &History,
    vhost: Option<&str>,
    limit: usize,
    refresh: &str,
) -> String {
    let mut out = format!(
        "roll queues  ·  {}  ·  up {}  ·  {refresh}\n\n",
        s(&o["node"]),
        fmt::span(o["uptime_s"].as_i64().unwrap_or(0))
    );
    for (k, buf) in [
        ("publish", &hist.publish),
        ("deliver", &hist.deliver),
        ("ack", &hist.ack),
    ] {
        let xs: Vec<f64> = buf.iter().copied().collect();
        out.push_str(&format!(
            "  {k:<8} {:>8}/s  {}\n",
            rate(f(&o["rates"][k])),
            fmt::spark(&xs)
        ));
    }
    let t = &o["totals"];
    out.push_str(&format!(
        "\n  ready {}   unacked {}   connections {}   channels {}   consumers {}   stored {}\n\n",
        count(f(&t["messages_ready"])),
        count(f(&t["messages_unacked"])),
        count(f(&t["connections"])),
        count(f(&t["channels"])),
        count(f(&t["consumers"])),
        fmt::size(f(&t["stored_bytes"])),
    ));
    let mut qs = match queues {
        Ok(qs) => qs,
        Err(e) => {
            out.push_str(&format!("  queues unavailable: {e:#}\n"));
            return out;
        }
    };
    if qs.is_empty() {
        out.push_str("  no queues\n");
        return out;
    }
    let depth = |x: &Value| f(&x["messages_ready"]) + f(&x["messages_unacked"]);
    qs.sort_by(|a, b| {
        depth(b)
            .total_cmp(&depth(a))
            .then(f(&b["rates"]["publish"]).total_cmp(&f(&a["rates"]["publish"])))
            .then(s(&a["name"]).cmp(&s(&b["name"])))
    });
    let mut cols = vec![];
    if vhost.is_none() {
        cols.push(("vhost", L));
    }
    cols.extend([
        ("queue", L),
        ("type", L),
        ("ready", R),
        ("unacked", R),
        ("consumers", R),
        ("pub/s", R),
        ("del/s", R),
        ("ack/s", R),
    ]);
    let mut table = Table::new(&cols);
    for x in qs.iter().take(limit) {
        let mut row = vec![];
        if vhost.is_none() {
            row.push(s(&x["vhost"]));
        }
        row.extend([
            fmt::truncate(&s(&x["name"]), 40),
            s(&x["type"]),
            count(f(&x["messages_ready"])),
            count(f(&x["messages_unacked"])),
            count(f(&x["consumers"])),
            rate(f(&x["rates"]["publish"])),
            rate(f(&x["rates"]["deliver"])),
            rate(f(&x["rates"]["ack"])),
        ]);
        table.row(row);
    }
    for line in table.render().lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    if qs.len() > limit {
        out.push_str(&format!("  … {} more\n", qs.len() - limit));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deepest_queues_first_and_limited() {
        let o = json!({ "node": "rolld@local", "uptime_s": 65, "rates": { "publish": 12.0, "deliver": 3.0, "ack": 3.0 },
                        "totals": { "messages_ready": 1500 } });
        let mut hist = History::default();
        hist.push(&o["rates"]);
        let q = |name: &str, ready: u64| json!({ "vhost": "/", "name": name, "type": "classic", "messages_ready": ready, "messages_unacked": 0 });
        let text = render(
            &o,
            Ok(vec![q("small", 1), q("big", 1000), q("mid", 499)]),
            &hist,
            Some("/"),
            2,
            "every 2s",
        );
        assert!(text.contains("up 1m 5s"), "{text}");
        assert!(text.contains("ready 1,500"), "{text}");
        let big = text.find("big").unwrap();
        let mid = text.find("mid").unwrap();
        assert!(big < mid);
        assert!(!text.contains("small"));
        assert!(text.contains("… 1 more"));
        assert!(!text.contains("vhost"));
    }

    #[test]
    fn history_is_bounded() {
        let mut h = History::default();
        for i in 0..(HISTORY + 10) {
            h.push(&json!({ "publish": i }));
        }
        assert_eq!(h.publish.len(), HISTORY);
        assert_eq!(h.publish.back(), Some(&((HISTORY + 9) as f64)));
    }
}
