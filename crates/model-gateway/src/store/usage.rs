//! Hourly usage roll-ups, and the report that sums them by a group into hours or days.

use super::{DAY, HOUR, cut, from_sql, sql_int};
use crate::Record;
use crate::price::cost_micros;
use agent_proto::{ModelPrice, UsageBucket, UsageBy, UsageReport, UsageRow, gateway_limits};
use rusqlite::{Connection, Transaction, params};

/// Whether a request counts as an error in usage (and the log): the client got 400 or more,
/// or the gateway said why it failed.
pub(super) fn is_error(r: &Record) -> bool {
    r.status >= 400 || r.error.is_some()
}

/// Adds `r` to its hour's row; returns its cost (micro-dollars), priced by the model the
/// answering upstream was asked for (an alias's target), else the model the client asked for.
pub(super) fn add(tx: &Transaction<'_>, r: &Record, prices: &[ModelPrice]) -> anyhow::Result<u64> {
    let last = r.attempts.last();
    let upstream = last.map_or("", |a| a.upstream.as_str());
    let upstream_model = last.map_or("", |a| a.model.as_str());
    let priced = if upstream_model.is_empty() { r.model.as_str() } else { upstream_model };
    let cost = cost_micros(priced, r.usage, prices);
    let key = |s: Option<&str>| cut(s.unwrap_or(""), gateway_limits::MODEL).to_string();
    tx.prepare_cached(
        "INSERT INTO usage (hour, client, machine, project, thread, model, upstream, upstream_model,
                            requests, errors, input, output, cache_read, cache_write, cost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT (hour, client, machine, project, thread, model, upstream, upstream_model) DO UPDATE SET
             requests = requests + 1,
             errors = errors + excluded.errors,
             input = input + excluded.input,
             output = output + excluded.output,
             cache_read = cache_read + excluded.cache_read,
             cache_write = cache_write + excluded.cache_write,
             cost = cost + excluded.cost",
    )?
    .execute(params![
        sql_int(r.at - r.at % HOUR),
        key(r.client.as_ref().map(|c| c.id.as_str())),
        key(r.client.as_ref().and_then(|c| c.machine.as_ref()).map(|m| m.as_str())),
        key(r.project.as_deref()),
        key(r.thread.as_deref()),
        key(Some(&r.model)),
        key(Some(upstream)),
        key(Some(upstream_model)),
        i64::from(is_error(r)),
        sql_int(r.usage.input),
        sql_int(r.usage.output),
        sql_int(r.usage.cache_read),
        sql_int(r.usage.cache_write),
        sql_int(cost),
    ])?;
    Ok(cost)
}

pub(super) fn report(db: &Connection, from: u64, to: u64, by: UsageBy, bucket: UsageBucket) -> anyhow::Result<UsageReport> {
    // `Model` groups by the model that answered (an alias's target), which is what was
    // priced; a request no upstream answered falls back to the model asked for.
    let key = match by {
        UsageBy::Model => "CASE upstream_model WHEN '' THEN model ELSE upstream_model END",
        UsageBy::Machine => "machine",
        UsageBy::Project => "project",
        UsageBy::Thread => "thread",
        UsageBy::Upstream => "upstream",
    };
    let size = match bucket {
        UsageBucket::Hour => HOUR,
        UsageBucket::Day => DAY,
    };
    // Every hour that overlaps [from, to); the newest buckets first, one more row than the
    // cap to tell whether it was reached.
    let sql = format!(
        "SELECT (hour / ?1) * ?1 AS at, {key} AS k, sum(requests), sum(errors), sum(input), sum(output),
                sum(cache_read), sum(cache_write), sum(cost)
         FROM usage WHERE hour >= ?2 AND hour < ?3
         GROUP BY at, k ORDER BY at DESC, k LIMIT ?4"
    );
    let mut q = db.prepare_cached(&sql)?;
    let mut rows: Vec<UsageRow> = q
        .query_map(params![sql_int(size), sql_int(from - from % HOUR), sql_int(to), sql_int(gateway_limits::USAGE_ROWS as u64 + 1)], |r| {
            Ok(UsageRow {
                at: from_sql(r.get(0)?),
                key: r.get(1)?,
                requests: from_sql(r.get(2)?),
                errors: from_sql(r.get(3)?),
                input_tokens: from_sql(r.get(4)?),
                output_tokens: from_sql(r.get(5)?),
                cache_read_tokens: from_sql(r.get(6)?),
                cache_write_tokens: from_sql(r.get(7)?),
                cost_micros: from_sql(r.get(8)?),
            })
        })?
        .collect::<Result<_, _>>()?;
    let truncated = rows.len() > gateway_limits::USAGE_ROWS;
    rows.truncate(gateway_limits::USAGE_ROWS);
    // Oldest first for charts; the cut above dropped the oldest.
    rows.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.key.cmp(&b.key)));
    Ok(UsageReport { by, bucket, from, to, rows, truncated })
}
