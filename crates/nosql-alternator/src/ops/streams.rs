//! The DynamoDB Streams API: ListStreams, DescribeStream, GetShardIterator, GetRecords.
//!
//! Formats follow ScyllaDB's (whose streams come from CDC):
//!
//! - StreamArn: `arn:aws:dynamodb:us-east-1:000000000000:table/alternator_T@T_scylla_cdc_log/stream/<time>`;
//!   only the table part is checked.
//! - ShardId: `H <generation ms, hex>:<32 hex digits of the stream id>`, one per CDC stream (vnode
//!   or tablet), as the backend lists them ([`Backend::stream_shards`]).
//! - SequenceNumber: the decimal of a 128-bit timeuuid-like value: the 60-bit timestamp (100 ns
//!   since 1582-10-15) in the high half, the clock and node bits in the low half
//!   ([`crate::backend::stream_position`]).
//! - ShardIterator: `I` (inclusive) or `i` (exclusive), the table id, the position as a UUID,
//!   and the shard id, joined by `:`.

use super::*;
use crate::backend::{StreamRecord, stream_position as position};
use crate::ops::table::{stream_arn, stream_label};

const MAX_RECORDS: u64 = 1000;

fn record_position(r: &StreamRecord) -> u128 {
    r.position
}

fn malformed(what: &str, v: &Json) -> Error {
    Error::validation(format!("Malformed value {what} : {}", v.to_json_string()))
}

/// The table named by a stream ARN.
async fn table_of_stream(alt: &Alternator, v: &Json) -> Result<Arc<TableSchema>> {
    let Some(arn) = v.as_str() else {
        return Err(malformed("StreamArn", v));
    };
    let parts = arn
        .split_once("table/")
        .and_then(|(_, rest)| rest.split_once("/stream/"))
        .and_then(|(t, _)| t.split_once('@'))
        .and_then(|(ks, t)| Some((ks, t.strip_suffix("_scylla_cdc_log")?)));
    let Some((ks, table)) = parts else {
        return Err(malformed("StreamArn", v));
    };
    // A well-formed ARN naming a keyspace that isn't its table's names no stream.
    if ks != format!("alternator_{table}") {
        return Err(Error::not_found(format!(
            "`{arn}` is not a valid StreamArn - table {table}_scylla_cdc_log not found"
        )));
    }
    match alt.backend.table(table).await? {
        Some(s) if s.stream.is_some() => Ok(s),
        _ => Err(Error::not_found(format!(
            "`{arn}` is not a valid StreamArn - table {table}_scylla_cdc_log not found"
        ))),
    }
}

/// ListStreams / DescribeStream `Limit`: a positive integer, no upper bound.
fn limit(body: &Json) -> Result<usize> {
    let Some(v) = body.get("Limit") else {
        return Ok(usize::MAX);
    };
    match v
        .as_number()
        .filter(|n| !n.is_double())
        .and_then(|n| n.as_i64())
    {
        Some(n) if n >= 1 => Ok(n as usize),
        Some(_) => Err(Error::validation("Limit must be 1 or more")),
        None => Err(malformed("Limit", v)),
    }
}

pub async fn list_streams(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let tables: Vec<Arc<TableSchema>> = match body.get("TableName") {
        Some(_) => vec![get_table(alt, body).await?],
        None => {
            let mut v = Vec::new();
            for n in alt.backend.list_tables().await? {
                if let Some(t) = alt.backend.table(&n).await? {
                    v.push(t);
                }
            }
            v
        }
    };
    let limit = limit(body)?;
    let start = match body.get("ExclusiveStartStreamArn") {
        None => None,
        Some(v) => {
            let ok = v
                .as_str()
                .is_some_and(|s| s.contains("table/") && s.contains("/stream/"));
            if !ok {
                return Err(malformed("ExclusiveStartStreamArn", v));
            }
            v.as_str().map(str::to_string)
        }
    };
    let all: Vec<(String, &TableSchema)> = tables
        .iter()
        .filter_map(|t| {
            t.stream
                .as_ref()
                .map(|s| (stream_arn(&t.name, s.created_at_ms), &**t))
        })
        .collect();
    // After the start ARN; when its table is gone, after where it was (tables are listed by
    // name).
    let table_of = |arn: &str| {
        arn.split_once('@')
            .and_then(|(_, r)| r.split_once("_scylla_cdc_log/"))
            .map(|(t, _)| t.to_string())
    };
    let skip = match &start {
        None => 0,
        Some(a) => match all.iter().position(|(x, _)| x == a) {
            Some(i) => i + 1,
            None => match table_of(a) {
                Some(gone) => all
                    .iter()
                    .position(|(_, t)| t.name.as_str() > gone.as_str())
                    .unwrap_or(all.len()),
                None => all.len(),
            },
        },
    };
    let page: Vec<&(String, &TableSchema)> = all.iter().skip(skip).take(limit).collect();
    let list = page
        .iter()
        .map(|(arn, t)| {
            let created = t.stream.as_ref().map_or(0, |s| s.created_at_ms);
            Json::object()
                .with("StreamArn", Json::str(arn.clone()))
                .with("StreamLabel", Json::str(stream_label(created)))
                .with("TableName", Json::str(t.name.clone()))
        })
        .collect();
    let mut out = Json::object().with("Streams", Json::Array(list));
    if (skip + page.len() < all.len() || (limit != usize::MAX && page.len() == limit))
        && let Some((arn, _)) = page.last()
    {
        out.push("LastEvaluatedStreamArn", Json::str(arn.clone()));
    }
    Ok(out)
}

pub async fn describe_stream(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let Some(arn) = body.get("StreamArn") else {
        return Err(Error::validation("JSON parameter StreamArn not found"));
    };
    let t = table_of_stream(alt, arn).await?;
    let limit = limit(body)?;
    let st = t.stream.clone().unwrap_or(crate::schema::StreamSpec {
        view_type: "KEYS_ONLY".into(),
        created_at_ms: 0,
        enabled: false,
        visible_at_ms: 0,
        reported_disabled: false,
    });
    let mut all = alt.backend.stream_shards(&t).await?;
    // ShardFilter: only CHILD_SHARDS, of a shard of the stream: the next generation's shards
    // that cover part of its token range.
    if let Some(f) = body.get("ShardFilter") {
        match f.get("Type").and_then(Json::as_str) {
            Some("CHILD_SHARDS") => {}
            Some(t) => {
                return Err(Error::validation(format!(
                    "ShardFilter.Type {t} is not supported: only CHILD_SHARDS is"
                )));
            }
            None => return Err(Error::validation("ShardFilter.Type is missing")),
        }
        let Some(id) = f.get("ShardId").and_then(Json::as_str) else {
            return Err(Error::validation("ShardFilter.ShardId is missing"));
        };
        let valid = id
            .strip_prefix("H ")
            .and_then(|r| r.split_once(':'))
            .is_some_and(|(g, s)| {
                u64::from_str_radix(g, 16).is_ok() && s.len() == 32 && hex::decode(s).is_ok()
            });
        if !valid {
            return Err(Error::validation(format!(
                "Invalid ShardFilter.ShardId value: {id}"
            )));
        }
        let children: Vec<String> = all
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.children.clone())
            .unwrap_or_default();
        all.retain(|s| children.contains(&s.id));
    }
    // Shards after ExclusiveStartShardId (none if it is unknown).
    let rest: Vec<&crate::backend::StreamShard> = match body.get("ExclusiveStartShardId") {
        None => all.iter().collect(),
        Some(v) => match all.iter().position(|s| Some(s.id.as_str()) == v.as_str()) {
            Some(i) => all[i + 1..].iter().collect(),
            None => Vec::new(),
        },
    };
    let page: Vec<&crate::backend::StreamShard> = rest.iter().take(limit).copied().collect();
    let mut shards = Vec::with_capacity(page.len());
    for sh in &page {
        let mut range =
            Json::object().with("StartingSequenceNumber", Json::str(sh.start.to_string()));
        if let Some(end) = sh.end {
            // A shard of an older generation is closed.
            range.push("EndingSequenceNumber", Json::str(end.to_string()));
        } else if !st.enabled {
            let last = alt
                .backend
                .stream_records(&t, &sh.id, 0, true, i64::MAX, usize::MAX)
                .await?;
            let end = last.last().map_or(sh.start, record_position);
            range.push("EndingSequenceNumber", Json::str(end.to_string()));
        }
        let mut shard = Json::object().with("ShardId", Json::str(sh.id.clone()));
        if let Some(p) = &sh.parent {
            shard.push("ParentShardId", Json::str(p.clone()));
        }
        shard.push("SequenceNumberRange", range);
        shards.push(shard);
    }
    let mut desc = Json::object()
        .with(
            "StreamStatus",
            Json::str(if st.enabled { "ENABLED" } else { "DISABLED" }),
        )
        .with("StreamArn", arn.clone())
        .with("StreamViewType", Json::str(st.view_type.clone()))
        .with("TableName", Json::str(t.name.clone()))
        .with("KeySchema", t.key.to_json());
    if limit != usize::MAX
        && !page.is_empty()
        && page.len() >= limit
        && let Some(last) = page.last()
    {
        desc.push("LastEvaluatedShardId", Json::str(last.id.clone()));
    }
    desc.push("Shards", Json::Array(shards));
    Ok(Json::object().with("StreamDescription", desc))
}

fn uuid_of(p: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(p)
}

fn iterator(inclusive: bool, t: &TableSchema, pos: u128, shard: &str) -> String {
    format!(
        "{}{}:{}:{shard}",
        if inclusive { 'I' } else { 'i' },
        t.id,
        uuid_of(pos)
    )
}

pub async fn get_shard_iterator(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let Some(ty) = body.get("ShardIteratorType") else {
        return Err(Error::validation(
            "JSON parameter ShardIteratorType not found",
        ));
    };
    let Some(arn) = body.get("StreamArn") else {
        return Err(Error::validation("JSON parameter StreamArn not found"));
    };
    let kind = match ty.as_str() {
        Some(k @ ("TRIM_HORIZON" | "LATEST" | "AT_SEQUENCE_NUMBER" | "AFTER_SEQUENCE_NUMBER")) => k,
        _ => return Err(malformed("ShardIteratorType", ty)),
    };
    let seq = body.get("SequenceNumber");
    let seq = match (kind, seq) {
        ("AT_SEQUENCE_NUMBER" | "AFTER_SEQUENCE_NUMBER", None) => {
            return Err(Error::validation(
                "Missing required parameter \"SequenceNumber\"",
            ));
        }
        ("AT_SEQUENCE_NUMBER" | "AFTER_SEQUENCE_NUMBER", Some(v)) => {
            let n = v
                .as_str()
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .map(|s| s.parse::<u128>().unwrap_or(u128::MAX));
            Some(n.ok_or_else(|| malformed("SequenceNumber", v))?)
        }
        (_, Some(_)) => {
            return Err(Error::validation(
                "Iterator of this type should not have sequence number",
            ));
        }
        (_, None) => None,
    };
    let t = table_of_stream(alt, arn).await?;
    let shard = match body.get("ShardId").and_then(Json::as_str) {
        Some(s) if s.starts_with("H ") && s.contains(':') => s.to_string(),
        _ => return Err(Error::not_found("Invalid ShardId")),
    };
    let it = match (kind, seq) {
        ("TRIM_HORIZON", _) => iterator(true, &t, 0, &shard),
        ("LATEST", _) => iterator(true, &t, position(alt.now_ms(), 0), &shard),
        ("AT_SEQUENCE_NUMBER", Some(n)) => iterator(true, &t, n, &shard),
        (_, n) => iterator(false, &t, n.unwrap_or(0), &shard),
    };
    Ok(Json::object().with("ShardIterator", Json::str(it)))
}

pub async fn get_records(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let Some(itv) = body.get("ShardIterator") else {
        return Err(Error::validation("JSON parameter ShardIterator not found"));
    };
    let parsed = itv.as_str().and_then(|s| {
        let inclusive = match s.as_bytes().first()? {
            b'I' => true,
            b'i' => false,
            _ => return None,
        };
        let rest = &s[1..];
        let table = uuid::Uuid::parse_str(rest.get(..36)?).ok()?;
        let rest = rest.get(36..)?.strip_prefix(':')?;
        let pos = uuid::Uuid::parse_str(rest.get(..36)?).ok()?.as_u128();
        let shard = rest.get(36..)?.strip_prefix(':')?.to_string();
        if !(shard.starts_with("H ") && shard.contains(':')) {
            return None;
        }
        Some((inclusive, table, pos, shard))
    });
    let Some((inclusive, table_id, pos, shard)) = parsed else {
        return Err(malformed("ShardIterator", itv));
    };
    let limit = match body.get("Limit") {
        None => MAX_RECORDS,
        Some(v) => match v
            .as_number()
            .filter(|n| !n.is_double())
            .and_then(|n| n.as_u64())
        {
            Some(0) => return Err(Error::validation("Limit must be 1 or more")),
            Some(n) if n > MAX_RECORDS => {
                return Err(Error::validation(
                    "Limit must be less than or equal to 1000",
                ));
            }
            Some(n) => n,
            None => return Err(malformed("Limit", v)),
        },
    };
    let mut table = None;
    for n in alt.backend.list_tables().await? {
        if let Some(t) = alt.backend.table(&n).await?
            && t.id == table_id
        {
            table = Some(t);
            break;
        }
    }
    let Some(t) = table else {
        return Err(Error::not_found(table_id.to_string()));
    };
    let log = format!("{}_scylla_cdc_log", t.name);
    super::authorize(
        alt,
        req,
        crate::backend::Permission::Select,
        crate::backend::Resource::of(&t.name, &log),
    )
    .await?;
    let view = t
        .stream
        .as_ref()
        .map_or("KEYS_ONLY".to_string(), |s| s.view_type.clone());
    // Records become readable once the streams time window has passed.
    let horizon = alt.now_ms() - alt.config().streams_time_window_ms;
    let all = alt
        .backend
        .stream_records(&t, &shard, pos, inclusive, horizon, limit as usize)
        .await?;
    let records: Vec<&StreamRecord> = all.iter().collect();
    let region = alt
        .config()
        .local_nodes
        .first()
        .map_or("datacenter1".to_string(), |n| n.datacenter.clone());
    let list = records
        .iter()
        .map(|r| record_json(r, &view, &region))
        .collect();
    let mut out = Json::object().with("Records", Json::Array(list));
    req.stats.table(&t.name);
    req.stats.returned_records(&t.name, records.len() as u64);
    // A closed shard (disabled stream) read to its end has no next iterator.
    let open = t.stream.as_ref().is_some_and(|s| s.enabled);
    match records.last() {
        Some(r) => out.push(
            "NextShardIterator",
            Json::str(iterator(false, &t, record_position(r), &shard)),
        ),
        None if open => out.push("NextShardIterator", itv.clone()),
        None => {}
    }
    req.stats.item_size(&t.name, out.to_json_string().len());
    Ok(out)
}

fn record_json(r: &StreamRecord, view: &str, region: &str) -> Json {
    let name = match r.kind {
        crate::backend::ChangeKind::Insert => "INSERT",
        crate::backend::ChangeKind::Modify => "MODIFY",
        crate::backend::ChangeKind::Remove => "REMOVE",
    };
    let mut d = Json::object()
        .with("Keys", item_json(&r.keys))
        .with("ApproximateCreationDateTime", Json::i64(r.time_ms / 1000))
        .with("SequenceNumber", Json::str(record_position(r).to_string()))
        .with("StreamViewType", Json::str(view));
    if matches!(view, "OLD_IMAGE" | "NEW_AND_OLD_IMAGES")
        && let Some(o) = &r.old
    {
        d.push("OldImage", item_json(o));
    }
    if matches!(view, "NEW_IMAGE" | "NEW_AND_OLD_IMAGES")
        && let Some(n) = &r.new
    {
        d.push("NewImage", item_json(n));
    }
    let mut out = Json::object()
        .with("eventName", Json::str(name))
        .with("awsRegion", Json::str(region))
        .with(
            "eventID",
            Json::str(uuid_of(record_position(r)).to_string()),
        )
        .with("eventSource", Json::str("scylladb:alternator"))
        .with("eventVersion", Json::str("1.1"))
        .with("dynamodb", d);
    if r.expired {
        out.push(
            "userIdentity",
            Json::object()
                .with("Type", Json::str("Service"))
                .with("PrincipalId", Json::str("dynamodb.amazonaws.com")),
        );
    }
    out
}
