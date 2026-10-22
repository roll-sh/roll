//! Table operations: CreateTable, DescribeTable, DeleteTable, UpdateTable, ListTables,
//! DescribeEndpoints, DescribeContinuousBackups.
//!
//! CreateTable and UpdateTable answer with the request itself as the `TableDescription`
//! (unknown members included), followed by `CreationDateTime`, `TableStatus` and `TableId`,
//! as ScyllaDB does; DescribeTable and DeleteTable describe the stored schema.

use super::*;
use crate::hashorder::unordered_set_order;
use crate::ops::tags::{parse_create_tags, validate_write_isolation};
use crate::schema::{IndexKind, IndexSchema, KeySchema, ScalarType, StreamSpec};

/// How long a GSI added by UpdateTable stays `CREATING`, and how long a stream enabled by
/// UpdateTable takes to show its ARN.
const SETTLE_MS: i64 = 1000;

fn seconds(ms: i64) -> Json {
    Json::f64(ms as f64 / 1000.0)
}

/// Key attribute names are at most 255 bytes (checked before anything else about the keys,
/// definitions first): `where` is the request part (`""`, ` - in GlobalSecondaryIndexes`, ...).
fn check_name_lengths(
    defs: Option<&Json>,
    keys: &[(Option<&Json>, &str)],
    defs_where: &str,
) -> Result<()> {
    const TOO_LARGE: &str = "Attribute name is too large, must be less than 256 bytes";
    let long = |v: &Json| {
        v.get("AttributeName")
            .and_then(Json::as_str)
            .is_some_and(|n| n.len() > 255)
    };
    if defs
        .and_then(Json::as_array)
        .is_some_and(|a| a.iter().any(long))
    {
        return Err(Error::validation(format!(
            "in AttributeDefinitions{defs_where} - {TOO_LARGE}"
        )));
    }
    for (ks, where_) in keys {
        for k in ks.and_then(|k| k.as_array()).unwrap_or_default() {
            if long(k) {
                let ty = match k.get("KeyType").and_then(Json::as_str) {
                    Some("RANGE") => "RANGE",
                    _ => "HASH",
                };
                return Err(Error::validation(format!(
                    "{ty} key in KeySchema{where_} - {TOO_LARGE}"
                )));
            }
        }
    }
    Ok(())
}

/// The KeySchemas of a CreateTable request's table and indexes, with where they are.
fn create_key_schemas(body: &Json) -> Vec<(Option<&Json>, &'static str)> {
    let mut v = vec![(body.get("KeySchema"), "")];
    for (member, label) in [
        ("GlobalSecondaryIndexes", " - in GlobalSecondaryIndexes"),
        ("LocalSecondaryIndexes", " - in Local Secondary Index"),
    ] {
        for i in body
            .get(member)
            .and_then(Json::as_array)
            .unwrap_or_default()
        {
            v.push((i.get("KeySchema"), label));
        }
    }
    v
}

/// A KeySchema array: `(name, is_range)` entries checked as ScyllaDB does.
fn parse_key_schema(v: Option<&Json>) -> Result<Vec<String>> {
    let Some(v) = v else {
        return Err(Error::validation("Missing KeySchema member"));
    };
    let a = match v.as_array() {
        Some(a) if (1..=2).contains(&a.len()) => a,
        _ => {
            return Err(Error::validation(
                "KeySchema must list exactly one or two key columns",
            ));
        }
    };
    let mut names = Vec::new();
    for (i, k) in a.iter().enumerate() {
        let (which, want) = if i == 0 {
            ("First", "HASH")
        } else {
            ("Second", "RANGE")
        };
        let Some(name) = k.get("AttributeName").and_then(Json::as_str) else {
            return Err(Error::validation(format!(
                "{which} key in KeySchema must have string AttributeName"
            )));
        };
        if k.get("KeyType").and_then(Json::as_str) != Some(want) {
            return Err(Error::validation(format!(
                "{which} key in KeySchema must be a {want} key"
            )));
        }
        names.push(name.to_string());
    }
    Ok(names)
}

/// The AttributeDefinitions of a request: name → type string, in order.
fn parse_attribute_definitions(v: &Json) -> Result<Vec<(String, String)>> {
    let Some(a) = v.as_array() else {
        return Err(Error::validation("AttributeDefinitions must be an array"));
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for d in a {
        let Some(n) = d.get("AttributeName") else {
            return Err(Error::validation(
                "AttributeName missing in AttributeDefinitions",
            ));
        };
        let Some(n) = n.as_str() else {
            return Err(Error::validation(
                "AttributeName in AttributeDefinitions must be a string",
            ));
        };
        if let Err(e) = crate::expr::resolve::check_attr_name(n) {
            return Err(Error::validation(format!("in AttributeDefinitions - {e}")));
        }
        let Some(t) = d.get("AttributeType") else {
            return Err(Error::validation(
                "AttributeType missing in AttributeDefinitions",
            ));
        };
        let Some(t) = t.as_str() else {
            return Err(Error::validation(
                "AttributeType in AttributeDefinitions must be a string",
            ));
        };
        if out.iter().any(|(x, _)| x == n) {
            return Err(Error::validation(format!(
                "Duplicate AttributeName={n} in AttributeDefinitions"
            )));
        }
        out.push((n.to_string(), t.to_string()));
    }
    Ok(out)
}

fn key_attr(defs: &[(String, String)], name: &str) -> Result<KeyAttr> {
    let Some((_, t)) = defs.iter().find(|(n, _)| n == name) else {
        return Err(Error::validation(format!(
            "KeySchema key '{name}' missing in AttributeDefinitions"
        )));
    };
    let Some(ty) = ScalarType::parse(t) else {
        return Err(Error::validation(format!(
            "Invalid key type '{t}', can only be S, B or N."
        )));
    };
    if name == crate::storage::ATTRS_COLUMN {
        return Err(Error::validation(
            "Column name ':attrs' is currently reserved. FIXME.",
        ));
    }
    Ok(KeyAttr {
        name: name.to_string(),
        ty,
    })
}

fn key_schema(defs: &[(String, String)], names: &[String]) -> Result<KeySchema> {
    let hash = key_attr(defs, &names[0])?;
    let range = names.get(1).map(|n| key_attr(defs, n)).transpose()?;
    Ok(KeySchema { hash, range })
}

/// An index definition from CreateTable or UpdateTable's `Create`.
struct IndexDef {
    name: String,
    key_names: Vec<String>,
    projection: Json,
    provisioned: Option<(u64, u64)>,
    warm: Option<Json>,
}

fn check_index_name(table: &str, name: &str, kind: IndexKind) -> Result<()> {
    if name.len() < 3 {
        return Err(Error::validation(
            "IndexName must be at least 3 characters long",
        ));
    }
    if !valid_name_chars(name) {
        return Err(Error::validation(format!(
            "IndexName '{name}' must satisfy regular expression pattern: [a-zA-Z0-9_.-]+"
        )));
    }
    let max = if kind == IndexKind::Global { 221 } else { 220 };
    if table.len() + name.len() > max {
        return Err(Error::validation(format!(
            "The total length of TableName ('{table}') and IndexName ('{name}') cannot exceed {max} characters"
        )));
    }
    Ok(())
}

fn parse_index(table: &str, v: &Json, kind: IndexKind, what: &str) -> Result<IndexDef> {
    if !v.is_object() {
        return Err(json_assert("IsObject()"));
    }
    let Some(name) = v.get("IndexName").and_then(Json::as_str) else {
        return Err(Error::validation(format!(
            "{what} IndexName must be a string."
        )));
    };
    check_index_name(table, name, kind)?;
    let key_names = parse_key_schema(v.get("KeySchema"))?;
    let provisioned = v.get("ProvisionedThroughput").and_then(|p| {
        let r = p.get("ReadCapacityUnits")?.as_number()?.as_u64()?;
        let w = p.get("WriteCapacityUnits")?.as_number()?.as_u64()?;
        Some((r, w))
    });
    Ok(IndexDef {
        name: name.to_string(),
        key_names,
        projection: v.get("Projection").cloned().unwrap_or(Json::object()),
        provisioned,
        warm: v.get("WarmThroughput").cloned(),
    })
}

fn parse_indexes(body: &Json, table: &str, member: &str, kind: IndexKind) -> Result<Vec<IndexDef>> {
    let Some(v) = body.get(member) else {
        return Ok(Vec::new());
    };
    let Some(a) = v.as_array() else {
        return Err(Error::validation(format!("{member} must be an array.")));
    };
    a.iter()
        .map(|x| parse_index(table, x, kind, member))
        .collect()
}

/// `(rcu, wcu)` of a ProvisionedThroughput object.
fn provisioned_throughput(pt: &Json) -> Result<(i64, i64)> {
    let field = |name: &str, lower: &str| -> Result<i64> {
        let Some(v) = pt.get(name) else {
            return Err(Error::validation(format!(
                "provisionedThroughput.{lower} is missing, when BillingMode=PROVISIONED, ProvisionedThroughput must be specified."
            )));
        };
        match v.as_number().and_then(|n| n.as_i32()) {
            Some(x) => Ok(x as i64),
            None => Err(Error::validation(format!(
                "Expected integer value for attribute {name}, got: {}",
                pt.to_json_string()
            ))),
        }
    };
    let r = field("ReadCapacityUnits", "readCapacityUnits")?;
    let w = field("WriteCapacityUnits", "writeCapacityUnits")?;
    Ok((r, w))
}

/// BillingMode and ProvisionedThroughput of CreateTable.
fn billing(body: &Json) -> Result<(String, (i64, i64))> {
    let mode = match body.get("BillingMode") {
        None => "PROVISIONED".to_string(),
        Some(Json::String(s)) => s.clone(),
        Some(_) => {
            return Err(Error::validation(format!(
                "Expected string value for attribute BillingMode, got: {}",
                body.to_json_string()
            )));
        }
    };
    match mode.as_str() {
        "PAY_PER_REQUEST" => {
            if body.get("ProvisionedThroughput").is_some() {
                return Err(Error::validation(
                    "When BillingMode=PAY_PER_REQUEST, ProvisionedThroughput cannot be specified.",
                ));
            }
            Ok((mode, (0, 0)))
        }
        "PROVISIONED" => {
            let Some(pt) = body.get("ProvisionedThroughput") else {
                return Err(Error::validation(
                    "When BillingMode=PROVISIONED, ProvisionedThroughput must be specified.",
                ));
            };
            Ok((mode, provisioned_throughput(pt)?))
        }
        _ => Err(Error::validation(
            "Unknown BillingMode={}. Must be PAY_PER_REQUEST or PROVISIONED.",
        )),
    }
}

const VIEW_TYPES: [&str; 4] = ["KEYS_ONLY", "NEW_IMAGE", "OLD_IMAGE", "NEW_AND_OLD_IMAGES"];

/// A StreamSpecification: `Some(Some(view type))` to enable, `Some(None)` to disable, `None`
/// when absent or not an object (ignored).
fn stream_spec(v: Option<&Json>) -> Result<Option<Option<String>>> {
    let Some(v) = v.filter(|v| v.is_object()) else {
        return Ok(None);
    };
    let Some(enabled) = v.get("StreamEnabled").and_then(Json::as_bool) else {
        return Err(Error::validation(
            "StreamSpecification needs boolean StreamEnabled",
        ));
    };
    if !enabled {
        return Ok(Some(None));
    }
    let vt = match v.get("StreamViewType") {
        None => "KEYS_ONLY".to_string(),
        Some(Json::String(s)) if VIEW_TYPES.contains(&s.as_str()) => s.clone(),
        Some(other) => {
            return Err(Error::validation(format!(
                "Malformed value StreamViewType : {}",
                other.to_json_string()
            )));
        }
    };
    Ok(Some(Some(vt)))
}

fn sse(v: Option<&Json>) -> Result<()> {
    let Some(v) = v.filter(|v| v.is_object()) else {
        return Ok(());
    };
    match v.get("Enabled") {
        Some(Json::Bool(false)) => Ok(()),
        Some(Json::Bool(true)) => Err(Error::validation(
            "SSESpecification: configuring encryption-at-rest is not yet supported.",
        )),
        _ => Err(Error::validation("SSESpecification needs boolean Enabled")),
    }
}

fn build_index(defs: &[(String, String)], d: IndexDef, kind: IndexKind) -> Result<IndexSchema> {
    Ok(IndexSchema {
        name: d.name,
        kind,
        key: key_schema(defs, &d.key_names)?,
        projection: d.projection,
        provisioned: d.provisioned,
        warm: d.warm,
        ready_at_ms: 0,
    })
}

/// A GSI keyed twice by one attribute (ScyllaDB crashes on it; refused here).
fn same_key_check(idx: &IndexSchema) -> Result<()> {
    if idx
        .key
        .range
        .as_ref()
        .is_some_and(|r| r.name == idx.key.hash.name)
    {
        return Err(Error::validation(format!(
            "GlobalSecondaryIndex {} uses the same attribute {} as its hash and range key",
            idx.name, idx.key.hash.name
        )));
    }
    Ok(())
}

/// The stream ARN of a table whose stream was enabled at `ms`.
pub(crate) fn stream_arn(table: &str, ms: i64) -> String {
    let t = chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default();
    format!(
        "arn:aws:dynamodb:us-east-1:000000000000:table/alternator_{table}@{table}_scylla_cdc_log/stream/{}",
        t.format("%Y-%m-%dT%H:%M:%S%.9f")
    )
}

pub(crate) fn stream_label(ms: i64) -> String {
    let t = chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default();
    format!("\"{}\"", t.format("%Y-%m-%dT%H:%M:%SZ"))
}

pub async fn create_table(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let name = table_name(body)?;
    check_table_name(name)?;
    if name.starts_with(INTERNAL_PREFIX) {
        return Err(Error::validation(
            "Prefix .scylla.alternator. is reserved for accessing internal tables",
        ));
    }
    let Some(ad) = body.get("AttributeDefinitions") else {
        return Err(Error::validation(
            "Missing AttributeDefinitions in CreateTable request",
        ));
    };
    if ad.as_array().is_none() {
        return Err(Error::validation("AttributeDefinitions must be an array"));
    }
    check_name_lengths(Some(ad), &create_key_schemas(body), "")?;
    let key_names = parse_key_schema(body.get("KeySchema"))?;
    let defs = parse_attribute_definitions(ad)?;
    let key = key_schema(&defs, &key_names)?;
    let (billing_mode, provisioned) = billing(body)?;
    let gsi_defs = parse_indexes(body, name, "GlobalSecondaryIndexes", IndexKind::Global)?;
    let lsi_defs = parse_indexes(body, name, "LocalSecondaryIndexes", IndexKind::Local)?;
    let mut gsis = Vec::new();
    for d in gsi_defs {
        let idx = build_index(&defs, d, IndexKind::Global)?;
        same_key_check(&idx)?;
        gsis.push(idx);
    }
    let mut lsis = Vec::new();
    for d in lsi_defs {
        let idx = build_index(&defs, d, IndexKind::Local)?;
        if idx.key.hash.name != key.hash.name {
            return Err(Error::validation(
                "LocalSecondaryIndex hash key must match the base table hash key",
            ));
        }
        if key.range.is_none() {
            return Err(Error::validation(
                "LocalSecondaryIndex requires that the base table have a range key",
            ));
        }
        if idx.key.range.is_none() {
            return Err(Error::validation(
                "LocalSecondaryIndex must specify a sort key",
            ));
        }
        if idx
            .key
            .range
            .as_ref()
            .is_some_and(|r| r.name == idx.key.hash.name)
        {
            return Err(Error::validation(format!(
                "LocalSecondaryIndex {} uses the same attribute {} as its hash and range key",
                idx.name, idx.key.hash.name
            )));
        }
        lsis.push(idx);
    }
    let mut seen: Vec<&str> = Vec::new();
    for i in gsis.iter().chain(&lsis) {
        if seen.contains(&i.name.as_str()) {
            return Err(Error::validation(format!(
                "Duplicate IndexName '{}', ",
                i.name
            )));
        }
        seen.push(&i.name);
    }
    // Every definition must be used by some key.
    let used: Vec<&str> = key
        .names()
        .chain(gsis.iter().chain(&lsis).flat_map(|i| i.key.names()))
        .collect();
    let spurious: Vec<&str> = defs
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !used.contains(n))
        .collect();
    if !spurious.is_empty() {
        let list: Vec<String> = unordered_set_order(&spurious)
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect();
        return Err(Error::validation(format!(
            "AttributeDefinitions defines spurious attributes not used by any KeySchema: {{{}}}",
            list.join(", ")
        )));
    }
    let tags = parse_create_tags(body.get("Tags"))?;
    if body
        .get("Tags")
        .and_then(Json::as_array)
        .is_some_and(|a| a.len() > 50)
    {
        return Err(Error::validation(
            "Number of Tags exceed the current limit for the provided ResourceArn",
        ));
    }
    if let Some(v) = tags
        .iter()
        .find(|(k, _)| k == "system:write_isolation")
        .map(|(_, v)| v)
    {
        validate_write_isolation(v)?;
    }
    let stream = stream_spec(body.get("StreamSpecification"))?;
    sse(body.get("SSESpecification"))?;
    let vector = super::vector::parse_create(body, &key, &gsis, &lsis, &tags)?;

    let now = alt.now_ms();
    let mut attribute_definitions: Vec<KeyAttr> = Vec::new();
    // ScyllaDB collects them from the key, the LSIs, then the GSIs (DescribeTable lists them
    // in the hash order of that insertion sequence).
    for k in std::iter::once(&key)
        .chain(lsis.iter().map(|i| &i.key))
        .chain(gsis.iter().map(|i| &i.key))
    {
        for a in std::iter::once(&k.hash).chain(k.range.as_ref()) {
            if !attribute_definitions.iter().any(|x| x.name == a.name) {
                attribute_definitions.push(a.clone());
            }
        }
    }
    // `tablets_mode_for_new_keyspaces`: without the tag, tablets unless `disabled`; `enforced`
    // refuses vnodes.
    let mode = alt
        .option_overrides()
        .get("tablets_mode_for_new_keyspaces")
        .map(|m| m.trim().trim_matches('"').to_ascii_lowercase())
        .unwrap_or_else(|| "enabled".into());
    let tagged = tags.iter().any(|(k, _)| k == "system:initial_tablets");
    let tablets = if tagged {
        super::vector::initial_tablets(&tags).is_some()
    } else {
        mode != "disabled"
    };
    if !tablets && mode == "enforced" {
        return Err(Error::validation(
            "Cannot create a table using vnodes: tablets_mode_for_new_keyspaces is enforced, so tables must use tablets",
        ));
    }
    let id = uuid::Uuid::now_v1(&[0x5a, 0x11, 0x0a, 0x7e, 0x00, 0x01]);
    let stream = match (&stream, &vector) {
        (Some(Some(vt)), _) => Some(StreamSpec {
            view_type: vt.clone(),
            created_at_ms: now,
            enabled: true,
            visible_at_ms: now,
            reported_disabled: false,
        }),
        (_, Some(v)) if !v.is_empty() => Some(StreamSpec {
            view_type: "KEYS_ONLY".into(),
            created_at_ms: now,
            enabled: false,
            visible_at_ms: now,
            reported_disabled: false,
        }),
        _ => None,
    };
    let options = Vec::new();
    let schema = TableSchema {
        name: name.to_string(),
        id,
        created_at_ms: now,
        key,
        attribute_definitions,
        gsis,
        lsis,
        billing_mode,
        billing_changed_ms: now,
        provisioned,
        stream: stream.clone(),
        tags,
        ttl_attribute: None,
        options,
        vector_indexes: vector.unwrap_or_default(),
        tablets,
    };
    super::authorize(
        alt,
        req,
        crate::backend::Permission::Create,
        crate::backend::Resource::AllKeyspaces,
    )
    .await?;
    let views: Vec<String> = schema
        .gsis
        .iter()
        .chain(&schema.lsis)
        .map(|i| i.view_name(name))
        .collect();
    if !alt.backend.create_table(schema).await? {
        return Err(Error::in_use(format!("Table {name} already exists")));
    }
    if let Some(role) = req.user {
        for t in std::iter::once(name.to_string()).chain(views) {
            alt.backend
                .grant_creator(role, &crate::backend::Resource::of(name, &t))
                .await?;
        }
    }
    let mut desc = body.clone();
    if let Some(Some(vt)) = stream_spec(body.get("StreamSpecification")).ok().flatten()
        && let Some(s) = desc.get_mut("StreamSpecification")
        && s.get("StreamViewType").is_none()
    {
        s.push("StreamViewType", Json::str(vt));
    }
    desc.push("CreationDateTime", seconds(now));
    desc.push("TableStatus", Json::str("ACTIVE"));
    desc.push("TableId", Json::str(id.to_string()));
    if let Some(s) = &stream {
        desc.push(
            "LatestStreamArn",
            Json::str(stream_arn(name, s.created_at_ms)),
        );
        desc.push(
            "LatestStreamLabel",
            Json::str(stream_label(s.created_at_ms)),
        );
        // ScyllaDB appends the actual specification after the echoed one.
        if body.get("StreamSpecification").is_none() || !s.enabled {
            desc.push(
                "StreamSpecification",
                Json::object()
                    .with("StreamEnabled", Json::Bool(s.enabled))
                    .with("StreamViewType", Json::str(s.view_type.clone())),
            );
        }
    }
    Ok(Json::object().with("TableDescription", desc))
}

fn key_schema_json(k: &KeySchema) -> Json {
    k.to_json()
}

/// DescribeTable's `Table` (and DeleteTable's, with `deleting`).
pub(crate) fn describe(s: &TableSchema, deleting: bool, now: i64, size: u64) -> Json {
    let mut t = Json::object()
        .with("TableName", Json::str(s.name.clone()))
        .with("TableSizeBytes", Json::u64(size))
        .with(
            "TableStatus",
            Json::str(if deleting { "DELETING" } else { "ACTIVE" }),
        )
        .with("TableArn", Json::str(s.arn()))
        .with("TableId", Json::str(s.id.to_string()))
        .with(
            "BillingModeSummary",
            Json::object()
                .with(
                    "LastUpdateToPayPerRequestDateTime",
                    seconds(s.billing_changed_ms),
                )
                .with("BillingMode", Json::str(s.billing_mode.clone())),
        )
        .with(
            "ProvisionedThroughput",
            Json::object()
                .with("ReadCapacityUnits", Json::i64(s.provisioned.0))
                .with("WriteCapacityUnits", Json::i64(s.provisioned.1))
                .with("NumberOfDecreasesToday", Json::u64(0)),
        );
    if !deleting {
        t.push("CreationDateTime", seconds(s.created_at_ms));
        t.push("KeySchema", key_schema_json(&s.key));
        let index_json = |i: &IndexSchema| {
            let mut j = Json::object()
                .with("IndexName", Json::str(i.name.clone()))
                .with(
                    "IndexArn",
                    Json::str(format!("{}/index/{}", s.arn(), i.name)),
                )
                .with("KeySchema", key_schema_json(&i.key))
                .with(
                    "Projection",
                    Json::object().with("ProjectionType", Json::str("ALL")),
                );
            if i.kind == IndexKind::Global {
                if now < i.ready_at_ms {
                    j.push("IndexStatus", Json::str("CREATING"));
                    j.push("Backfilling", Json::Bool(true));
                } else {
                    j.push("IndexStatus", Json::str("ACTIVE"));
                }
            }
            j
        };
        if !s.lsis.is_empty() {
            t.push(
                "LocalSecondaryIndexes",
                Json::Array(s.lsis.iter().map(index_json).collect()),
            );
        }
        if !s.gsis.is_empty() {
            t.push(
                "GlobalSecondaryIndexes",
                Json::Array(s.gsis.iter().map(index_json).collect()),
            );
        }
        let names: Vec<&str> = s
            .attribute_definitions
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        let order = unordered_set_order(&names);
        let defs = order
            .iter()
            .filter_map(|n| s.attribute_definitions.iter().find(|a| a.name == *n))
            .map(|a| {
                Json::object()
                    .with("AttributeName", Json::str(a.name.clone()))
                    .with("AttributeType", Json::str(a.ty.name()))
            })
            .collect();
        t.push("AttributeDefinitions", Json::Array(defs));
        if !s.vector_indexes.is_empty() {
            t.push("VectorIndexes", super::vector::describe(&s.vector_indexes));
        }
    }
    if let Some(st) = &s.stream {
        if deleting || now >= st.visible_at_ms || !st.enabled {
            t.push(
                "LatestStreamArn",
                Json::str(stream_arn(&s.name, st.created_at_ms)),
            );
            t.push(
                "LatestStreamLabel",
                Json::str(stream_label(st.created_at_ms)),
            );
        }
        t.push(
            "StreamSpecification",
            Json::object()
                .with(
                    "StreamEnabled",
                    Json::Bool(st.enabled && !st.reported_disabled),
                )
                .with("StreamViewType", Json::str(st.view_type.clone())),
        );
    }
    t
}

pub async fn describe_table(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let s = get_table(alt, req.body).await?;
    let size = alt.table_size(&s).await?;
    Ok(Json::object().with("Table", describe(&s, false, alt.now_ms(), size)))
}

pub async fn delete_table(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let name = table_name(req.body)?;
    let s = get_table_named(alt, name).await?;
    authorize_table(alt, req, crate::backend::Permission::Drop, name).await?;
    alt.backend.delete_table(name).await?;
    Ok(Json::object().with("TableDescription", describe(&s, true, alt.now_ms(), 0)))
}

pub async fn list_tables(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let start_name = match body.get("ExclusiveStartTableName") {
        None => None,
        Some(Json::String(s)) => Some(s.clone()),
        Some(v) => {
            return Err(Error::internal(format!(
                "Internal server error: std::runtime_error (rjson::to_string expects a string value, got: {})",
                rapid_type(v)
            )));
        }
    };
    let limit = match body.get("Limit") {
        None => 100,
        Some(v) => match v.as_number().and_then(|n| n.as_i64()) {
            Some(n) if (1..=100).contains(&n) => n as usize,
            Some(_) => {
                return Err(Error::validation(
                    "Limit must be greater than 0 and no greater than 100",
                ));
            }
            None => return Err(json_assert("IsInt()")),
        },
    };
    let names: Vec<String> = alt.backend.list_tables().await?;
    let start = match start_name {
        None => 0,
        Some(s) if s.is_empty() => 0,
        Some(s) => match names.iter().position(|n| *n == s) {
            Some(i) => i + 1,
            None => names.len(),
        },
    };
    let page: Vec<&String> = names.iter().skip(start).take(limit).collect();
    let mut out = Json::object().with(
        "TableNames",
        Json::Array(page.iter().map(|n| Json::str(n.as_str())).collect()),
    );
    if start + page.len() < names.len()
        && let Some(last) = page.last()
    {
        out.push("LastEvaluatedTableName", Json::str(last.as_str()));
    }
    Ok(out)
}

pub async fn describe_endpoints(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let address = match &alt.config().describe_endpoints {
        Some(a) if !a.is_empty() => a.clone(),
        _ => match req.host {
            Some(h) => h.to_string(),
            None => {
                return Err(Error::validation(
                    "DescribeEndpoints needs a 'Host:' header in request",
                ));
            }
        },
    };
    Ok(Json::object().with(
        "Endpoints",
        Json::Array(vec![
            Json::object()
                .with("Address", Json::str(address))
                .with("CachePeriodInMinutes", Json::u64(1440)),
        ]),
    ))
}

pub async fn describe_continuous_backups(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let name = table_name(req.body)?;
    check_table_name(name)?;
    if alt.backend.table(name).await?.is_none() {
        return Err(Error::new(
            ErrorKind::TableNotFound,
            format!("Table {name} not found"),
        ));
    }
    Ok(Json::object().with(
        "ContinuousBackupsDescription",
        Json::object()
            .with("ContinuousBackupsStatus", Json::str("DISABLED"))
            .with(
                "PointInTimeRecoveryDescription",
                Json::object().with("PointInTimeRecoveryStatus", Json::str("DISABLED")),
            ),
    ))
}

pub async fn update_table(alt: &Alternator, req: &Request<'_>) -> Result<Json> {
    let body = req.body;
    let name = table_name(body)?;
    let billing_mode = match body.get("BillingMode") {
        None => None,
        Some(Json::String(s)) if s == "PAY_PER_REQUEST" || s == "PROVISIONED" => Some(s.clone()),
        Some(Json::String(_)) => {
            return Err(Error::validation(
                "Unknown BillingMode={}. Must be PAY_PER_REQUEST or PROVISIONED.",
            ));
        }
        Some(_) => {
            return Err(Error::validation(format!(
                "Expected string value for attribute BillingMode, got: {}",
                body.to_json_string()
            )));
        }
    };
    let schema = get_table_named(alt, name).await?;
    authorize_table(alt, req, crate::backend::Permission::Alter, name).await?;
    let mut s = (*schema).clone();
    if body.get("ProvisionedThroughput").is_some() {
        return Err(Error::validation("ProvisionedThroughput not supported"));
    }
    if body.get("SSESpecification").is_some() {
        return Err(Error::validation("SSESpecification not supported"));
    }
    if body.get("ReplicaUpdates").is_some() {
        return Err(Error::validation("ReplicaUpdates not supported"));
    }
    let gsi_updates = body
        .get("GlobalSecondaryIndexUpdates")
        .filter(|v| v.as_array().is_none_or(|a| !a.is_empty()));
    let vector_updates = body
        .get("VectorIndexUpdates")
        .filter(|v| v.as_array().is_none_or(|a| !a.is_empty()));
    let stream = body.get("StreamSpecification").filter(|v| v.is_object());
    if gsi_updates.is_none()
        && vector_updates.is_none()
        && stream.is_none()
        && billing_mode.is_none()
    {
        return Err(Error::validation(
            "UpdateTable requires one of GlobalSecondaryIndexUpdates, VectorIndexUpdates, StreamSpecification or BillingMode to be specified",
        ));
    }
    if let Some(m) = billing_mode {
        if m == "PROVISIONED" {
            return Err(Error::validation(
                "When BillingMode=PROVISIONED, ProvisionedThroughput must be specified.",
            ));
        }
        if m != s.billing_mode {
            s.billing_mode = m;
            s.billing_changed_ms = alt.now_ms();
        }
    }
    let old_stream = s.stream.clone();
    let mut echo_view_type = None;
    let mut new_vnode_stream: Option<StreamSpec> = None;
    if let Some(spec) = stream_spec(stream)? {
        match spec {
            Some(vt) => {
                if s.stream.as_ref().is_some_and(|x| x.enabled) {
                    return Err(Error::validation(format!(
                        "Table already has an enabled stream: TableName: {name}"
                    )));
                }
                if !s.vector_indexes.is_empty() && vt == "KEYS_ONLY" {
                    return Err(Error::internal(super::vector::KEYS_ONLY_STREAM_ERROR));
                }
                echo_view_type = Some(vt.clone());
                let now = alt.now_ms();
                // A stream on tablets shows its ARN once its CDC streams exist; on vnodes, or for
                // a vector table whose CDC log exists already, at once.
                let delay = if s.tablets && s.vector_indexes.is_empty() {
                    SETTLE_MS
                } else {
                    0
                };
                let reported_disabled = !s.vector_indexes.is_empty();
                s.stream = Some(StreamSpec {
                    view_type: vt,
                    created_at_ms: now,
                    enabled: true,
                    visible_at_ms: now + delay,
                    reported_disabled,
                });
                if !s.tablets {
                    new_vnode_stream = s.stream.clone();
                }
            }
            None => {
                let Some(cur) = s.stream.as_mut().filter(|x| x.enabled) else {
                    return Err(Error::validation(format!(
                        "Table has no stream to disable: TableName: {name}"
                    )));
                };
                cur.enabled = false;
                echo_view_type = Some(cur.view_type.clone());
            }
        }
    }
    if let Some(u) = gsi_updates {
        update_gsis(&mut s, body, u, alt.now_ms())?;
    }
    if let Some(u) = vector_updates {
        super::vector::update(&mut s, u, gsi_updates.is_some(), alt.now_ms())?;
    }
    if !alt.backend.alter_table(s.clone()).await? {
        return Err(table_not_found(name));
    }
    if let Some(role) = req.user {
        for i in s.gsis.iter().filter(|i| schema.index(&i.name).is_none()) {
            let view = crate::backend::Resource::of(name, &i.view_name(name));
            alt.backend.grant_creator(role, &view).await?;
        }
    }
    let mut desc = body.clone();
    if let (Some(vt), Some(spec)) = (echo_view_type.clone(), desc.get_mut("StreamSpecification"))
        && spec.get("StreamViewType").is_none()
    {
        spec.push("StreamViewType", Json::str(vt));
    }
    desc.push("CreationDateTime", seconds(s.created_at_ms));
    desc.push("TableStatus", Json::str("ACTIVE"));
    desc.push("TableId", Json::str(s.id.to_string()));
    if let Some(st) = &new_vnode_stream {
        desc.push(
            "LatestStreamArn",
            Json::str(stream_arn(name, st.created_at_ms)),
        );
        desc.push(
            "LatestStreamLabel",
            Json::str(stream_label(st.created_at_ms)),
        );
    }
    // A first vector index creates the (disabled) CDC log.
    if let (None, Some(st), None) = (&old_stream, &s.stream, &echo_view_type) {
        desc.push(
            "LatestStreamArn",
            Json::str(stream_arn(name, st.created_at_ms)),
        );
        desc.push(
            "LatestStreamLabel",
            Json::str(stream_label(st.created_at_ms)),
        );
        desc.push(
            "StreamSpecification",
            Json::object()
                .with("StreamEnabled", Json::Bool(st.enabled))
                .with("StreamViewType", Json::str(st.view_type.clone())),
        );
    }
    let has_vectors = !s.vector_indexes.is_empty();
    if let Some(st) = old_stream.filter(|st| st.enabled || has_vectors) {
        if desc.get("StreamSpecification").is_none() || has_vectors {
            let view = s
                .stream
                .as_ref()
                .map_or(st.view_type.clone(), |x| x.view_type.clone());
            let shown = s
                .stream
                .as_ref()
                .is_some_and(|x| x.enabled && !x.reported_disabled);
            desc.push(
                "StreamSpecification",
                Json::object()
                    .with("StreamEnabled", Json::Bool(shown))
                    .with("StreamViewType", Json::str(view)),
            );
        }
        desc.push(
            "LatestStreamArn",
            Json::str(stream_arn(name, st.created_at_ms)),
        );
        desc.push(
            "LatestStreamLabel",
            Json::str(stream_label(st.created_at_ms)),
        );
    }
    Ok(Json::object().with("TableDescription", desc))
}

fn update_gsis(s: &mut TableSchema, body: &Json, u: &Json, now: i64) -> Result<()> {
    let Some(list) = u.as_array() else {
        return Err(Error::validation(
            "GlobalSecondaryIndexUpdates must be an array",
        ));
    };
    if list.len() > 1 {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "GlobalSecondaryIndexUpdates only allows one index creation or deletion",
        ));
    }
    let one_op = || {
        Error::validation(
            "GlobalSecondaryIndexUpdates array must contain one object with a Create, Delete or Update operation",
        )
    };
    let Some(op) = list.first() else {
        return Err(one_op());
    };
    let Some([(kind, v)]) = op.as_object() else {
        return Err(one_op());
    };
    if !v.is_object() {
        return Err(Error::validation(
            "GlobalSecondaryIndexUpdates entries must be objects",
        ));
    }
    match kind.as_str() {
        "Create" => {
            if v.get("IndexName").and_then(Json::as_str).is_none() {
                return Err(Error::validation(
                    "GlobalSecondaryIndexUpdates operation must have IndexName",
                ));
            }
            check_name_lengths(
                body.get("AttributeDefinitions"),
                &[(v.get("KeySchema"), " - in GlobalSecondaryIndexUpdates")],
                " - in GlobalSecondaryIndexUpdates",
            )?;
            let d = parse_index(&s.name, v, IndexKind::Global, "GlobalSecondaryIndexUpdates")?;
            let Some(ad) = body.get("AttributeDefinitions") else {
                return Err(Error::validation(
                    "GlobalSecondaryIndexUpdates Create needs AttributeDefinitions",
                ));
            };
            let defs = parse_attribute_definitions(ad)?;
            for (n, _) in &defs {
                if s.vector_indexes.iter().any(|v| v.attribute == *n) {
                    return Err(Error::validation(format!(
                        "AttributeDefinitions redefines {n} but already a key of a vector index in this table"
                    )));
                }
            }
            for (n, t) in &defs {
                if let (Some(cur), Some(new)) = (s.attr_type(n), ScalarType::parse(t))
                    && cur != new
                {
                    return Err(Error::validation(format!(
                        "AttributeDefinitions redefined {n} to {} already a key attribute of type {} in this table",
                        new.name(),
                        cur.name()
                    )));
                }
            }
            let mut idx = build_index(&defs, d, IndexKind::Global)?;
            same_key_check(&idx)?;
            let spurious: Vec<&str> = defs
                .iter()
                .map(|(n, _)| n.as_str())
                .filter(|n| !idx.key.contains(n))
                .collect();
            if !spurious.is_empty() {
                let list: Vec<String> = unordered_set_order(&spurious)
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect();
                return Err(Error::validation(format!(
                    "AttributeDefinitions defines spurious attributes not used by any KeySchema: {{{}}}",
                    list.join(", ")
                )));
            }
            if s.gsis.iter().any(|g| g.name == idx.name) {
                return Err(Error::validation(format!(
                    "GSI {} already exists in table {}",
                    idx.name, s.name
                )));
            }
            if s.vector_indexes.iter().any(|v| v.name == idx.name) {
                return Err(Error::validation(format!(
                    "Vector index {} already exists in table {}, cannot reuse the name for a GSI",
                    idx.name, s.name
                )));
            }
            if s.lsis.iter().any(|g| g.name == idx.name) {
                return Err(Error::validation(format!(
                    "LSI {} already exists in table {}, can't use same name for GSI",
                    idx.name, s.name
                )));
            }
            if idx.key.range.is_none() {
                // An index added by UpdateTable gets the base hash key as its range key.
                idx.key.range = Some(s.key.hash.clone());
            }
            for a in std::iter::once(&idx.key.hash).chain(idx.key.range.as_ref()) {
                if !s.attribute_definitions.iter().any(|x| x.name == a.name) {
                    s.attribute_definitions.push(a.clone());
                }
            }
            idx.ready_at_ms = now + SETTLE_MS;
            s.gsis.push(idx);
            Ok(())
        }
        "Delete" => {
            let Some(n) = v.get("IndexName").and_then(Json::as_str) else {
                return Err(Error::validation(
                    "GlobalSecondaryIndexUpdates operation must have IndexName",
                ));
            };
            let Some(pos) = s.gsis.iter().position(|g| g.name == n) else {
                return Err(Error::not_found(format!("No GSI {n} in table {}", s.name)));
            };
            s.gsis.remove(pos);
            Ok(())
        }
        "Update" => Err(Error::validation(
            "GlobalSecondaryIndexUpdates Update not yet supported",
        )),
        other => Err(Error::validation(format!(
            "GlobalSecondaryIndexUpdates supports a Create, Delete or Update operation, saw '{other}'"
        ))),
    }
}
