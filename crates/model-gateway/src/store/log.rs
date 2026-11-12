//! The request log (one row per request, attempts as JSON) and its bodies.

use super::usage::is_error;
use super::{cut, from_sql, sql_int};
use crate::Record;
use agent_proto::{Attempt, LogBody, LogEntry, MachineId, WireFormat, gateway_limits};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

/// The most of a request or response body the log keeps (64 KiB).
pub const BODY_CAP: usize = 64 * 1024;
/// Attempts kept per request (a route has at most 16 targets; more is a bug, not history).
const ATTEMPTS: usize = 32;
/// An error message, the gateway's own or an attempt's.
const ERROR: usize = 1024;
/// A project or thread id from a header (validated by the server; bounded here too).
const ID: usize = 128;

/// Appends `chunk` to a body being kept for the log, stopping one byte past [`BODY_CAP`] so
/// [`crate::store::Store::record`] can tell a cut body (`truncated`) from one that fit.
pub fn keep_body(kept: &mut Vec<u8>, chunk: &[u8]) {
    let room = (BODY_CAP + 1).saturating_sub(kept.len());
    kept.extend_from_slice(&chunk[..chunk.len().min(room)]);
}

fn format_name(f: WireFormat) -> &'static str {
    match f {
        WireFormat::Messages => "messages",
        WireFormat::Chat => "chat",
        WireFormat::Responses => "responses",
    }
}

fn format_from(s: &str) -> WireFormat {
    match s {
        "chat" => WireFormat::Chat,
        "responses" => WireFormat::Responses,
        _ => WireFormat::Messages,
    }
}

pub(super) fn add(tx: &Transaction<'_>, r: &Record, cost: u64, log_bodies: bool) -> anyhow::Result<()> {
    let attempts: Vec<Attempt> = r
        .attempts
        .iter()
        .take(ATTEMPTS)
        .map(|a| Attempt {
            upstream: cut(&a.upstream, gateway_limits::ID).to_string(),
            model: cut(&a.model, gateway_limits::MODEL).to_string(),
            status: a.status,
            ms: a.ms,
            error: a.error.as_deref().map(|e| cut(e, ERROR).to_string()),
        })
        .collect();
    let failed = is_error(r) || r.attempts.len() > 1;
    tx.prepare_cached(
        "INSERT INTO log (at, client, machine, project, thread, format, model, stream, status, ms, first_byte_ms,
                          attempts, input, output, cache_read, cache_write, cost, error, failed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
    )?
    .execute(params![
        sql_int(r.at),
        r.client.as_ref().map(|c| cut(&c.id, ID)),
        r.client.as_ref().and_then(|c| c.machine.as_ref()).map(|m| cut(m.as_str(), ID)),
        r.project.as_deref().map(|p| cut(p, ID)),
        r.thread.as_deref().map(|t| cut(t, ID)),
        format_name(r.format),
        cut(&r.model, gateway_limits::MODEL),
        r.stream,
        r.status,
        r.ms,
        r.first_byte_ms,
        serde_json::to_string(&attempts)?,
        sql_int(r.usage.input),
        sql_int(r.usage.output),
        sql_int(r.usage.cache_read),
        sql_int(r.usage.cache_write),
        sql_int(cost),
        r.error.as_deref().map(|e| cut(e, ERROR)),
        failed,
    ])?;
    if log_bodies && let Some((request, response)) = &r.bodies {
        let truncated = request.len() > BODY_CAP || response.len() > BODY_CAP;
        tx.prepare_cached("INSERT INTO bodies (seq, at, request, response, truncated) VALUES (?1, ?2, ?3, ?4, ?5)")?.execute(params![
            tx.last_insert_rowid(),
            sql_int(r.at),
            cut(request, BODY_CAP),
            cut(response, BODY_CAP),
            truncated,
        ])?;
    }
    Ok(())
}

const COLUMNS: &str = "log.seq, log.at, log.client, log.machine, log.project, log.thread, log.format, log.model, log.stream,
    log.status, log.ms, log.first_byte_ms, log.attempts, log.input, log.output, log.cache_read, log.cache_write, log.cost,
    log.error, EXISTS (SELECT 1 FROM bodies WHERE bodies.seq = log.seq)";

fn entry(r: &Row<'_>) -> rusqlite::Result<LogEntry> {
    let attempts: String = r.get(12)?;
    Ok(LogEntry {
        seq: from_sql(r.get(0)?),
        at: from_sql(r.get(1)?),
        client: r.get(2)?,
        machine: r.get::<_, Option<String>>(3)?.map(MachineId),
        project: r.get(4)?,
        thread: r.get(5)?,
        format: format_from(&r.get::<_, String>(6)?),
        model: r.get(7)?,
        stream: r.get(8)?,
        status: r.get(9)?,
        ms: r.get(10)?,
        first_byte_ms: r.get(11)?,
        // Written by `add` above: only a corrupt row fails, and then it just has none.
        attempts: serde_json::from_str(&attempts).unwrap_or_default(),
        input_tokens: from_sql(r.get(13)?),
        output_tokens: from_sql(r.get(14)?),
        cache_read_tokens: from_sql(r.get(15)?),
        cache_write_tokens: from_sql(r.get(16)?),
        cost_micros: from_sql(r.get(17)?),
        error: r.get(18)?,
        has_body: r.get(19)?,
    })
}

pub(super) fn page(db: &Connection, after: Option<u64>, before: Option<u64>, limit: u16, errors: bool) -> anyhow::Result<Vec<LogEntry>> {
    let limit = match limit {
        0 => gateway_limits::LOG_PAGE,
        n => n.min(gateway_limits::LOG_PAGE),
    };
    // `failed` is indexed with `seq`, so the errors filter pages as cheaply as the whole log.
    let filter = if errors { "log.failed = 1 AND" } else { "" };
    let sql = format!("SELECT {COLUMNS} FROM log WHERE {filter} log.seq > ?1 AND log.seq < ?2 ORDER BY log.seq DESC LIMIT ?3");
    let mut q = db.prepare_cached(&sql)?;
    let rows = q.query_map(params![after.map_or(0, sql_int), before.map_or(i64::MAX, sql_int), limit], entry)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(super) fn body(db: &Connection, seq: u64) -> anyhow::Result<Option<LogBody>> {
    Ok(db
        .query_row("SELECT request, response, truncated FROM bodies WHERE seq = ?1", [sql_int(seq)], |r| {
            Ok(LogBody { seq, request: r.get(0)?, response: r.get(1)?, truncated: r.get(2)? })
        })
        .optional()?)
}
