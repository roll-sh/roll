//! TRACING ON and SHOW SESSION: the session trace table.
//!
//! Ported from Apache Cassandra's `pylib/cqlshlib/tracing.py` and the Python driver's
//! `QueryTrace.populate` / `TraceEvent` (`cassandra/query.py`, bundled with Cassandra; both
//! Apache-2.0). Event times come from the event ids (timeuuids) as float seconds; ScyllaDB's cqlsh
//! shows them timezone-aware (`+00:00`).

use std::time::{Duration, Instant};

use bytes::Bytes;
use nosql_proto::{Consistency, QueryParams, ResultBody, Rows};

use super::format::{self, Flavor, Fv};
use super::pyfmt;
use super::render;
use super::session::cell;
use super::shell::Shell;

/// A populated trace.
pub struct Trace {
    pub id: [u8; 16],
    pub request: Option<String>,
    /// Microseconds since the epoch.
    pub started_at: Option<i64>,
    pub duration: Option<i64>,
    pub coordinator: Option<String>,
    pub client: Option<String>,
    pub events: Vec<Event>,
}

pub struct Event {
    pub activity: Option<String>,
    pub micros: i64,
    pub source: Option<String>,
    pub elapsed: Option<i64>,
    pub thread: Option<String>,
}

fn text(rows: &Rows, row: &[Option<Bytes>], name: &str) -> Option<String> {
    cell(rows, row, name).map(|b| String::from_utf8_lossy(b).into_owned())
}

fn inet(rows: &Rows, row: &[Option<Bytes>], name: &str) -> Option<String> {
    cell(rows, row, name).and_then(|b| nosql_types::inet::format_inet(b, nosql_types::Dialect::Scylla))
}

fn int(rows: &Rows, row: &[Option<Bytes>], name: &str) -> Option<i64> {
    let b = cell(rows, row, name)?;
    match b.len() {
        4 => Some(i32::from_be_bytes(b[..].try_into().ok()?) as i64),
        8 => Some(i64::from_be_bytes(b[..].try_into().ok()?)),
        _ => None,
    }
}

/// `datetime.utcfromtimestamp(unix_time_from_uuid1(id))`, in microseconds.
pub fn event_micros(id: &[u8; 16]) -> i64 {
    let raw = nosql_types::timeuuid::raw_timestamp(id);
    pyfmt::utc_micros_from_float((raw - 0x01B2_1DD2_1381_4000) as f64 / 1e7)
}

async fn select(shell: &Shell, cql: String, cl: Consistency) -> Option<Rows> {
    let mut p = QueryParams::new(cl);
    p.page_size = Some(5000);
    match shell.conn.query(&cql, p, false).await.ok()?.body {
        ResultBody::Rows(r) => Some(r),
        _ => None,
    }
}

/// `QueryTrace.populate`: `None` is `TraceUnavailable`.
pub async fn populate(shell: &Shell, id: [u8; 16], max_wait: f64, wait_for_complete: bool, cl: Consistency) -> Option<Trace> {
    let uuid = nosql_types::timeuuid::format_uuid(&id);
    let start = Instant::now();
    let mut attempt = 0;
    loop {
        if start.elapsed().as_secs_f64() >= max_wait {
            return None;
        }
        let sessions = select(shell, format!("SELECT * FROM system_traces.sessions WHERE session_id = {uuid}"), cl).await;
        let row = sessions.as_ref().and_then(|r| r.to_vec().into_iter().next());
        let (Some(rows), Some(row)) = (sessions.as_ref(), row) else {
            tokio::time::sleep(Duration::from_secs_f64(0.003 * 2f64.powi(attempt))).await;
            attempt += 1;
            continue;
        };
        let duration = int(rows, &row, "duration");
        let started = cell(rows, &row, "started_at").and_then(|b| <[u8; 8]>::try_from(&b[..]).ok()).map(i64::from_be_bytes);
        let complete = duration.is_some() && started.is_some();
        if wait_for_complete && !complete {
            tokio::time::sleep(Duration::from_secs_f64(0.003 * 2f64.powi(attempt))).await;
            attempt += 1;
            continue;
        }
        let mut trace = Trace {
            id,
            request: text(rows, &row, "request"),
            started_at: started.map(|ms| pyfmt::timedelta_micros(ms as f64 / 1000.0)),
            duration: if complete { duration } else { None },
            coordinator: inet(rows, &row, "coordinator"),
            client: inet(rows, &row, "client"),
            events: vec![],
        };
        if let Some(ev) = select(shell, format!("SELECT * FROM system_traces.events WHERE session_id = {uuid}"), cl).await {
            for r in ev.to_vec() {
                let micros = cell(&ev, &r, "event_id")
                    .and_then(|b| <[u8; 16]>::try_from(&b[..]).ok())
                    .map(|u| event_micros(&u))
                    .unwrap_or(0);
                trace.events.push(Event {
                    activity: text(&ev, &r, "activity"),
                    micros,
                    source: inet(&ev, &r, "source"),
                    elapsed: int(&ev, &r, "source_elapsed"),
                    thread: text(&ev, &r, "thread"),
                });
            }
        }
        return Some(trace);
    }
}

fn opt_text(v: &Option<String>, shell: &Shell) -> Fv {
    match v {
        Some(s) => format::text_value(s, &shell.opts),
        None => format::format(&nosql_types::CqlType::Text, None, &shell.opts),
    }
}

/// `make_trace_rows`.
pub fn rows(trace: &Trace, shell: &Shell) -> Vec<Vec<Fv>> {
    if trace.events.is_empty() {
        return vec![];
    }
    let o = &shell.opts;
    let off = pyfmt::local_offset_micros();
    let aware = shell.flavor == Flavor::Scylla;
    let started = trace.started_at.map(|us| pyfmt::datetime_str(us + off, false));
    let mut out = vec![vec![
        opt_text(&trace.request, shell),
        opt_text(&started, shell),
        opt_text(&trace.coordinator, shell),
        format::int_value(0, o),
        opt_text(&trace.client, shell),
    ]];
    for e in &trace.events {
        let activity = format!(
            "{} [{}]",
            e.activity.as_deref().unwrap_or("None"),
            e.thread.as_deref().unwrap_or("None")
        );
        let elapsed = match e.elapsed {
            Some(n) if n != 0 => format::int_value(n, o),
            _ => format::text_value("--", o),
        };
        out.push(vec![
            format::text_value(&activity, o),
            format::text_value(&pyfmt::datetime_str(e.micros + off, aware), o),
            opt_text(&e.source, shell),
            elapsed,
            opt_text(&trace.client, shell),
        ]);
    }
    if let (Some(d), Some(s)) = (trace.duration.filter(|d| *d != 0), trace.started_at) {
        out.push(vec![
            format::text_value("Request complete", o),
            format::text_value(&pyfmt::datetime_str(s + off + d, false), o),
            opt_text(&trace.coordinator, shell),
            format::int_value(d, o),
            opt_text(&trace.client, shell),
        ]);
    }
    out
}

/// `print_trace`.
pub fn print_trace(shell: &mut Shell, trace: &Trace) {
    let rows = rows(trace, shell);
    let id = nosql_types::timeuuid::format_uuid(&trace.id);
    if rows.is_empty() {
        return shell.printerr(&format!("No rows for session {id} found."));
    }
    let names: Vec<Fv> =
        ["activity", "timestamp", "source", "source_elapsed", "client"].iter().map(|n| format::colname(n, &shell.opts)).collect();
    shell.writeresult("");
    let label = if shell.opts.color && shell.capture.is_none() {
        format!("{}Tracing session: {}", format::MAGENTA, format::RESET)
    } else {
        "Tracing session: ".into()
    };
    shell.write_query(&label);
    shell.writeresult(&id);
    shell.writeresult("");
    let color = shell.opts.color && shell.capture.is_none();
    let table = render::table(&names, Some(&rows), true, shell.tty, color);
    shell.write_query(&table);
    shell.writeresult("");
}

/// After a traced statement (`get_all_query_traces`).
pub async fn print_query_trace(shell: &mut Shell, id: [u8; 16]) {
    match populate(shell, id, shell.max_trace_wait, true, shell.consistency).await {
        Some(t) => print_trace(shell, &t),
        None => {
            let msg = format!(
                "Statement trace did not complete within {} seconds; trace data may be incomplete.",
                shell.max_trace_wait as i64
            );
            shell.writeresult_colored(&msg, format::RED);
            show_session(shell, id, true).await;
        }
    }
}

/// SHOW SESSION (`print_trace_session`; the driver's default wait of 2 s).
pub async fn show_session(shell: &mut Shell, id: [u8; 16], partial: bool) {
    match populate(shell, id, 2.0, !partial, Consistency::One).await {
        Some(t) => print_trace(shell, &t),
        None => {
            let msg = format!("Session {} wasn't found.", nosql_types::timeuuid::format_uuid(&id));
            shell.printerr(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_times_round_like_python() {
        // 93c355d0-ccc2-11f1-8c36-33d044eb584e: 2026-10-20 20:12:29.997 and change.
        let u = nosql_types::timeuuid::parse_uuid("93c355d0-ccc2-11f1-8c36-33d044eb584e").unwrap();
        let us = event_micros(&u);
        assert_eq!(pyfmt::datetime_str(us, false), "2026-10-20 20:12:29.997000");
        assert_eq!(pyfmt::datetime_str(us, true), "2026-10-20 20:12:29.997000+00:00");
    }
}
