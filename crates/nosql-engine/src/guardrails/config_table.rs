//! Writes to ScyllaDB's virtual tables: `UPDATE system.config SET value = ? WHERE name = ?`
//! changes a live-updatable option of the database ([`crate::Database::set_settings`], so the
//! next request sees it); the other virtual tables refuse writes.
//!
//! Every refusal is the `WriteFailure` (0x1500, one failed replica, the request's consistency)
//! the reference answers, checked in its order: a deletion or a null value fails the generic
//! way ("Operation failed for system.config - …"), then `option value is required`, `option
//! type is immutable`, `option source is not updateable`, `no such option`, `option is not
//! live-updateable`, `cannot parse option value`. An option set this way reads `source = 'cql'`.
//! `enable_cql_config_updates = false` makes `system.config` refuse writes like the others.
//! Cassandra's virtual tables refuse every write with "Modification is not supported by table
//! ks.t" (`AbstractVirtualTable.apply`).
//!
//! Which options are live-updatable: the ones the ScyllaDB suites update (Alternator's too:
//! listeners registered with [`Database::on_option_set`] hear every change), the audit filters
//! (`docs/operating-scylla/security/auditing.rst`), the large data thresholds (live per
//! `docs/cql/guardrails.rst`) and the restriction switches; every other option answers `option
//! is not live-updateable`.

use std::collections::BTreeMap;

use nosql_storage::{ColumnData, PartitionUpdate};

use nosql_types::Dialect;

use crate::config::{Settings, StrictAllowFiltering};
use crate::database::Database;
use crate::error::{Consistency, Error, ErrorCode, ErrorDetail, Result};
use crate::schema::Table;
use crate::system::rows_data::SCYLLA_CONFIG;

/// ScyllaDB's virtual tables (`system`; `docs/dev/system_keyspace.md`): writes to them are
/// prepared like any write and refused at execution.
pub const SCYLLA_VIRTUAL: &[&str] = &[
    "cluster_status",
    "clients",
    "config",
    "custom_indexes",
    "load_per_node",
    "protocol_servers",
    "runtime_info",
    "size_estimates",
    "snapshots",
    "tablet_sizes",
    "token_ring",
    "versions",
];

/// Live-updatable options without behaviour in roll: `system.config` keeps the value set.
const LIVE_VALUES: &[&str] = &[
    // Alternator's live options: Alternator follows them ([`Database::on_option_set`]).
    "alternator_describe_table_info_cache_validity_in_seconds",
    "alternator_enforce_authorization",
    "alternator_force_read_before_write",
    "alternator_http_response_disable_content_type_header",
    "alternator_http_response_disable_date_header",
    "alternator_http_response_server_header",
    "alternator_max_expression_cache_entries_per_shard",
    "alternator_max_users_query_size_in_trace_output",
    "alternator_streams_increased_compatibility",
    "alternator_warn_authorization",
    "max_concurrent_requests_per_shard",
    "auto_snapshot_ttl",
    "compaction_enforce_min_threshold",
    "compaction_large_data_records_per_sstable",
    "components_memory_reclaim_threshold",
    "query_page_size_in_bytes",
    "restrict_dtcs",
    "restrict_twcs_without_default_ttl",
    "select_internal_page_size",
    "strict_is_not_null_in_views",
    "user_defined_function_time_limit_ms",
    "vector_store_primary_uri",
    "vector_store_secondary_uri",
];

/// Whether a write to `t` is prepared (and refused or applied at execution) instead of failing
/// with "… keyspace is not user-modifiable." at preparation.
pub fn accepts_writes(db: &Database, t: &Table) -> bool {
    match db.dialect() {
        Dialect::Scylla => t.keyspace == "system" && SCYLLA_VIRTUAL.contains(&t.name.as_str()),
        // Cassandra's virtual keyspaces (`AbstractVirtualTable.apply`).
        Dialect::Cassandra => matches!(t.keyspace.as_str(), "system_views" | "system_virtual_schema"),
    }
}

/// A failed write as the reference answers it: one replica failed.
pub fn write_failure(
    db: &Database,
    message: impl Into<String>,
    consistency: Consistency,
    write_type: &str,
) -> Error {
    Error {
        detail: ErrorDetail::WriteFailure {
            consistency,
            received: 0,
            block_for: 1,
            num_failures: 1,
            reasons: vec![(db.cfg.address, 0)],
            write_type: write_type.into(),
        },
        ..Error::new(ErrorCode::WriteFailure, message)
    }
}

/// The failure of a write the virtual table can't take at all (deletions, conditions).
pub fn generic_failure(db: &Database, t: &Table, cl: Consistency, write_type: &str) -> Error {
    write_failure(
        db,
        format!(
            "Operation failed for {}.{} - received 0 responses and 1 failures from 1 CL={}.",
            t.keyspace,
            t.name,
            cl.name()
        ),
        cl,
        write_type,
    )
}

/// Applies the updates of a write to a ScyllaDB virtual table (`write_type`: `SIMPLE`,
/// `UNLOGGED_BATCH` for several partitions, `CAS` for a conditional statement).
pub fn apply(
    db: &Database,
    t: &Table,
    ups: &[PartitionUpdate],
    cl: Consistency,
    conditional: bool,
) -> Result<()> {
    let write_type = if conditional {
        "CAS"
    } else if ups.len() > 1 {
        "UNLOGGED_BATCH"
    } else {
        "SIMPLE"
    };
    if !db.dialect().is_scylla() {
        return Err(Error::invalid(format!(
            "Modification is not supported by table {}.{}",
            t.keyspace, t.name
        )));
    }
    let fail = |m: &str| write_failure(db, m, cl, write_type);
    let settings = db.settings();
    if t.name != "config" || !settings.scylla.cql_config_updates {
        return Err(fail("this virtual table doesn't allow updates"));
    }
    if conditional {
        return Err(generic_failure(db, t, cl, write_type));
    }
    let id = |name: &str| t.column_index(name).map(|i| t.columns[i].id);
    let (value_id, type_id, source_id) = (id("value"), id("type"), id("source"));
    for u in ups {
        let deleted = !u.deletion.is_live()
            || !u.range_tombstones.is_empty()
            || u.rows.iter().any(|r| !r.deletion.time.is_live());
        if deleted {
            return Err(generic_failure(db, t, cl, write_type));
        }
        let cell = |col: Option<u32>| {
            u.rows.iter().flat_map(|r| &r.columns).find_map(|c| match c {
                ColumnData::Simple { column, cell } if Some(*column) == col => Some(cell),
                _ => None,
            })
        };
        let Some(value) = cell(value_id) else {
            return Err(fail("option value is required"));
        };
        if value.is_tombstone() {
            return Err(generic_failure(db, t, cl, write_type));
        }
        if cell(type_id).is_some() {
            return Err(fail("option type is immutable"));
        }
        if cell(source_id).is_some() {
            return Err(fail("option source is not updateable"));
        }
        // `name` is the whole (single-column) partition key.
        let name = String::from_utf8_lossy(&u.key.key).to_string();
        let value = String::from_utf8_lossy(&value.value).to_string();
        let mut s = (*db.settings()).clone();
        set_value(&mut s, &name, &value).map_err(fail)?;
        let renames = s.scylla.duplicate_bind_names_same_variable
            != settings.scylla.duplicate_bind_names_same_variable
            || s.scylla.in_bind_name_uppercase != settings.scylla.in_bind_name_uppercase;
        let mut shown = BTreeMap::new();
        values(&s, &mut shown);
        // Only live settings change here, so a reload is never needed.
        let _ = db.set_settings(s);
        if renames {
            // Prepared statements' bind variables follow the preparation switches.
            db.evict_prepared(|_| true);
        }
        if let Some(v) = shown.get(name.as_str()) {
            db.option_set(&name, v);
        }
    }
    Ok(())
}

type Setter = fn(&mut Settings, &str) -> std::result::Result<(), &'static str>;

const PARSE: &str = "cannot parse option value";

/// The setter of option `name`, or why it can't be set.
fn set(_s: &mut Settings, name: &str) -> std::result::Result<Setter, &'static str> {
    let Some((_, _, ty, _)) = SCYLLA_CONFIG.iter().find(|(n, ..)| *n == name) else {
        return Err("no such option");
    };
    let f: Setter = match name {
        "batch_size_warn_threshold_in_kb" => |s, v| {
            s.batch_size_warn_threshold = uint(v)? * 1024;
            Ok(())
        },
        "batch_size_fail_threshold_in_kb" => |s, v| {
            s.batch_size_fail_threshold = uint(v)? * 1024;
            Ok(())
        },
        "tombstone_warn_threshold" => |s, v| {
            s.tombstone_warn_threshold = uint(v)?;
            Ok(())
        },
        "tombstone_failure_threshold" => |s, v| {
            s.tombstone_failure_threshold = uint(v)?;
            Ok(())
        },
        "enable_create_table_with_compact_storage" => |s, v| {
            s.compact_storage_enabled = boolean(v)?;
            Ok(())
        },
        "max_partition_key_restrictions_per_query" => |s, v| {
            s.max_partition_key_restrictions_per_query = uint(v)?;
            Ok(())
        },
        "max_clustering_key_restrictions_per_query" => |s, v| {
            s.max_clustering_key_restrictions_per_query = uint(v)?;
            Ok(())
        },
        "strict_allow_filtering" => |s, v| {
            s.strict_allow_filtering = match restriction(v)? {
                Restriction::True => StrictAllowFiltering::True,
                Restriction::False => StrictAllowFiltering::False,
                Restriction::Warn => StrictAllowFiltering::Warn,
            };
            Ok(())
        },
        "minimum_replication_factor_warn_threshold" => |s, v| {
            s.minimum_replication_factor_warn_threshold = int(v)?;
            Ok(())
        },
        "minimum_replication_factor_fail_threshold" => |s, v| {
            s.scylla.minimum_replication_factor_fail_threshold = int(v)?;
            Ok(())
        },
        "maximum_replication_factor_warn_threshold" => |s, v| {
            s.scylla.maximum_replication_factor_warn_threshold = int(v)?;
            Ok(())
        },
        "maximum_replication_factor_fail_threshold" => |s, v| {
            s.scylla.maximum_replication_factor_fail_threshold = int(v)?;
            Ok(())
        },
        "replication_strategy_warn_list" => |s, v| {
            s.scylla.replication_strategy_warn_list = strategies(v)?;
            Ok(())
        },
        "replication_strategy_fail_list" => |s, v| {
            s.scylla.replication_strategy_fail_list = strategies(v)?;
            Ok(())
        },
        "restrict_replication_simplestrategy" => |s, v| {
            restriction(v)?;
            s.scylla.restrict_replication_simplestrategy = v.to_string();
            Ok(())
        },
        "write_consistency_levels_warned" => |s, v| {
            s.scylla.write_consistency_levels_warned = levels(v)?;
            Ok(())
        },
        "write_consistency_levels_disallowed" => |s, v| {
            s.scylla.write_consistency_levels_disallowed = levels(v)?;
            Ok(())
        },
        "query_tombstone_page_limit" => |s, v| {
            s.scylla.query_tombstone_page_limit = uint(v)?;
            Ok(())
        },
        "compaction_large_partition_warning_threshold_mb" => |s, v| {
            s.scylla.large_data.partition_warn_mb = uint(v)?;
            Ok(())
        },
        "large_partition_fail_threshold_mb" => |s, v| {
            s.scylla.large_data.partition_fail_mb = uint(v)?;
            Ok(())
        },
        "compaction_rows_count_warning_threshold" => |s, v| {
            s.scylla.large_data.rows_warn = uint(v)?;
            Ok(())
        },
        "rows_count_fail_threshold" => |s, v| {
            s.scylla.large_data.rows_fail = uint(v)?;
            Ok(())
        },
        "compaction_large_row_warning_threshold_mb" => |s, v| {
            s.scylla.large_data.row_warn_mb = uint(v)?;
            Ok(())
        },
        "large_row_fail_threshold_mb" => |s, v| {
            s.scylla.large_data.row_fail_mb = uint(v)?;
            Ok(())
        },
        "compaction_large_cell_warning_threshold_mb" => |s, v| {
            s.scylla.large_data.cell_warn_mb = uint(v)?;
            Ok(())
        },
        "large_cell_fail_threshold_mb" => |s, v| {
            s.scylla.large_data.cell_fail_mb = uint(v)?;
            Ok(())
        },
        "compaction_collection_elements_count_warning_threshold" => |s, v| {
            s.scylla.large_data.collection_warn = uint(v)?;
            Ok(())
        },
        "large_collection_elements_fail_threshold" => |s, v| {
            s.scylla.large_data.collection_fail = uint(v)?;
            Ok(())
        },
        "large_data_cql_warnings" => |s, v| {
            s.scylla.large_data.cql_warnings = boolean(v)?;
            Ok(())
        },
        "enable_cassio_compatibility" => |s, v| {
            s.scylla.enable_cassio_compatibility = boolean(v)?;
            Ok(())
        },
        "cql_duplicate_bind_variable_names_refer_to_same_variable" => |s, v| {
            s.scylla.duplicate_bind_names_same_variable = boolean(v)?;
            Ok(())
        },
        "cql_in_bind_variable_name_uses_uppercase_operator" => |s, v| {
            s.scylla.in_bind_name_uppercase = boolean(v)?;
            Ok(())
        },
        "restrict_future_timestamp" => |s, v| {
            s.scylla.restrict_future_timestamp = boolean(v)?;
            Ok(())
        },
        "enable_cql_config_updates" => |s, v| {
            s.scylla.cql_config_updates = boolean(v)?;
            Ok(())
        },
        "audit_categories" => |s, v| {
            s.scylla.audit.set_categories(v);
            Ok(())
        },
        "audit_keyspaces" => |s, v| {
            s.scylla.audit.set_keyspaces(v);
            Ok(())
        },
        "audit_tables" => |s, v| {
            s.scylla.audit.set_tables(v);
            Ok(())
        },
        "audit_rules" => |s, v| s.scylla.audit.set_rules(v),
        n if LIVE_VALUES.contains(&n) => match *ty {
            "integer" => |s, v| store(s, "integer", v),
            "bool" => |s, v| store(s, "bool", v),
            "double" | "float" => |s, v| store(s, "double", v),
            "restriction mode" => |s, v| store(s, "restriction mode", v),
            _ => |s, v| store(s, "string", v),
        },
        _ => return Err("option is not live-updateable"),
    };
    Ok(f)
}

/// Keeps a live option's value as `system.config` shows it (the option is named by the last
/// `set` call: `store` runs right after it, see [`apply`]).
fn store(s: &mut Settings, ty: &str, v: &str) -> std::result::Result<(), &'static str> {
    let shown = match ty {
        "integer" => int(v)?.to_string(),
        "bool" => boolean(v)?.to_string(),
        "double" => {
            let f: f64 = v.parse().map_err(|_| PARSE)?;
            if !f.is_finite() {
                return Err(PARSE);
            }
            f.to_string()
        }
        "restriction mode" => {
            restriction(v)?;
            json_string(v)
        }
        _ => json_string(v),
    };
    s.scylla.values.insert(PENDING.to_string(), shown);
    Ok(())
}

/// The key `store` leaves its value under until [`set_value`] renames it.
const PENDING: &str = "";

/// Sets option `name` to `value` (the `UPDATE system.config` semantics without the table:
/// `Err` is the reference's message).
pub fn set_value(s: &mut Settings, name: &str, value: &str) -> std::result::Result<(), &'static str> {
    let f = set(s, name)?;
    // Values are YAML: a quoted scalar is its content (`"0"` as `system.config` shows it).
    let value = unquote(value);
    f(s, &value)?;
    if let Some(v) = s.scylla.values.remove(PENDING) {
        s.scylla.values.insert(name.to_string(), v);
    }
    s.scylla.cql_sourced.insert(name.to_string());
    Ok(())
}

/// A YAML scalar's value: the content of a double-quoted (JSON escapes) or single-quoted
/// string, anything else as written.
fn unquote(v: &str) -> std::borrow::Cow<'_, str> {
    if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        return v[1..v.len() - 1].replace("''", "'").into();
    }
    if !(v.len() >= 2 && v.starts_with('"') && v.ends_with('"')) {
        return v.into();
    }
    let mut out = String::new();
    let mut it = v[1..v.len() - 1].chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out.into()
}

fn uint(v: &str) -> std::result::Result<u64, &'static str> {
    if v.starts_with('+') {
        return Err(PARSE);
    }
    v.parse().map_err(|_| PARSE)
}

fn int(v: &str) -> std::result::Result<i64, &'static str> {
    if v.starts_with('+') {
        return Err(PARSE);
    }
    v.parse().map_err(|_| PARSE)
}

fn boolean(v: &str) -> std::result::Result<bool, &'static str> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(PARSE),
    }
}

/// ScyllaDB's `tri_mode_restriction`.
enum Restriction {
    True,
    False,
    Warn,
}

fn restriction(v: &str) -> std::result::Result<Restriction, &'static str> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(Restriction::True),
        "false" | "0" => Ok(Restriction::False),
        "warn" => Ok(Restriction::Warn),
        _ => Err(PARSE),
    }
}

/// Whether a restriction mode value is `true` / `warn`.
pub fn restriction_mode(v: &str) -> (bool, bool) {
    match restriction(v) {
        Ok(Restriction::True) => (true, false),
        Ok(Restriction::Warn) => (false, true),
        _ => (false, false),
    }
}

/// A list option: `[a, "b"]` (YAML flow sequence) or `a,b`; empty for `''` or `[]`.
fn list(v: &str) -> std::result::Result<Vec<String>, &'static str> {
    let t = v.trim();
    let inner = match t.strip_prefix('[') {
        Some(rest) => rest.strip_suffix(']').ok_or(PARSE)?,
        None => t,
    };
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|e| {
            let e = e.trim();
            let e = e
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .or_else(|| e.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
                .unwrap_or(e);
            if e.is_empty() {
                Err(PARSE)
            } else {
                Ok(e.to_string())
            }
        })
        .collect()
}

fn strategies(v: &str) -> std::result::Result<Vec<String>, &'static str> {
    let names = list(v)?;
    for n in &names {
        let short = n.rsplit('.').next().unwrap_or(n);
        if !matches!(
            short,
            "SimpleStrategy" | "NetworkTopologyStrategy" | "LocalStrategy" | "EverywhereStrategy"
        ) {
            return Err(PARSE);
        }
    }
    Ok(names
        .into_iter()
        .map(|n| n.rsplit('.').next().unwrap_or(&n).to_string())
        .collect())
}

fn levels(v: &str) -> std::result::Result<Vec<Consistency>, &'static str> {
    list(v)?
        .iter()
        .map(|n| {
            (0..12u16)
                .filter_map(Consistency::from_code)
                .find(|c| c.name().eq_ignore_ascii_case(n))
                .ok_or(PARSE)
        })
        .collect()
}

fn json_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for ch in v.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<String> = items.map(json_string).collect();
    format!("[{}]", items.join(","))
}

/// The `system.config` values of the options above (rows of options set to their default show
/// the image's value).
pub fn values(s: &Settings, o: &mut BTreeMap<&'static str, String>) {
    let g = &s.scylla;
    let l = &g.large_data;
    let mut put = |k: &'static str, v: String| {
        o.insert(k, v);
    };
    put(
        "strict_allow_filtering",
        json_string(match s.strict_allow_filtering {
            StrictAllowFiltering::True => "true",
            StrictAllowFiltering::False => "false",
            StrictAllowFiltering::Warn => "warn",
        }),
    );
    put(
        "alternator_ttl_period_in_seconds",
        g.alternator_ttl_period_in_seconds.to_string(),
    );
    put(
        "minimum_replication_factor_fail_threshold",
        g.minimum_replication_factor_fail_threshold.to_string(),
    );
    put(
        "maximum_replication_factor_warn_threshold",
        g.maximum_replication_factor_warn_threshold.to_string(),
    );
    put(
        "maximum_replication_factor_fail_threshold",
        g.maximum_replication_factor_fail_threshold.to_string(),
    );
    put(
        "replication_strategy_warn_list",
        json_list(g.replication_strategy_warn_list.iter().map(String::as_str)),
    );
    put(
        "replication_strategy_fail_list",
        json_list(g.replication_strategy_fail_list.iter().map(String::as_str)),
    );
    put(
        "restrict_replication_simplestrategy",
        json_string(&g.restrict_replication_simplestrategy),
    );
    put(
        "write_consistency_levels_warned",
        json_list(g.write_consistency_levels_warned.iter().map(|c| c.name())),
    );
    put(
        "write_consistency_levels_disallowed",
        json_list(g.write_consistency_levels_disallowed.iter().map(|c| c.name())),
    );
    put(
        "query_tombstone_page_limit",
        g.query_tombstone_page_limit.to_string(),
    );
    put(
        "compaction_large_partition_warning_threshold_mb",
        l.partition_warn_mb.to_string(),
    );
    put(
        "large_partition_fail_threshold_mb",
        l.partition_fail_mb.to_string(),
    );
    put("compaction_rows_count_warning_threshold", l.rows_warn.to_string());
    put("rows_count_fail_threshold", l.rows_fail.to_string());
    put(
        "compaction_large_row_warning_threshold_mb",
        l.row_warn_mb.to_string(),
    );
    put("large_row_fail_threshold_mb", l.row_fail_mb.to_string());
    put(
        "compaction_large_cell_warning_threshold_mb",
        l.cell_warn_mb.to_string(),
    );
    put("large_cell_fail_threshold_mb", l.cell_fail_mb.to_string());
    put(
        "compaction_collection_elements_count_warning_threshold",
        l.collection_warn.to_string(),
    );
    put(
        "large_collection_elements_fail_threshold",
        l.collection_fail.to_string(),
    );
    put("large_data_cql_warnings", l.cql_warnings.to_string());
    put(
        "enable_cassio_compatibility",
        g.enable_cassio_compatibility.to_string(),
    );
    put(
        "cql_duplicate_bind_variable_names_refer_to_same_variable",
        g.duplicate_bind_names_same_variable.to_string(),
    );
    put(
        "cql_in_bind_variable_name_uses_uppercase_operator",
        g.in_bind_name_uppercase.to_string(),
    );
    put(
        "restrict_future_timestamp",
        g.restrict_future_timestamp.to_string(),
    );
    put("enable_cql_config_updates", g.cql_config_updates.to_string());
    put("audit_categories", json_string(&g.audit.categories_text));
    put("audit_keyspaces", json_string(&g.audit.keyspaces_text));
    put("audit_tables", json_string(&g.audit.tables_text));
    put("audit_rules", g.audit.rules_text());
    for (k, v) in &g.values {
        if let Some((n, ..)) = SCYLLA_CONFIG.iter().find(|(n, ..)| n == k) {
            put(n, v.clone());
        }
    }
}

/// The `source` of option `name` in `system.config`.
pub fn source<'a>(s: &Settings, name: &str, default: &'a str) -> &'a str {
    if s.scylla.cql_sourced.contains(name) {
        "cql"
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosql_types::Dialect;

    fn settings() -> Settings {
        Settings::defaults(Dialect::Scylla)
    }

    #[test]
    fn parse_and_show() {
        let mut s = settings();
        set_value(&mut s, "query_tombstone_page_limit", "10").unwrap();
        assert_eq!(s.scylla.query_tombstone_page_limit, 10);
        assert_eq!(set_value(&mut s, "query_tombstone_page_limit", "abc"), Err(PARSE));
        assert_eq!(set_value(&mut s, "query_tombstone_page_limit", " 5"), Err(PARSE));
        assert_eq!(set_value(&mut s, "query_tombstone_page_limit", "-5"), Err(PARSE));
        assert_eq!(set_value(&mut s, "no_such_option", "1"), Err("no such option"));
        assert_eq!(
            set_value(&mut s, "cluster_name", "x"),
            Err("option is not live-updateable")
        );
        assert_eq!(
            set_value(&mut s, "num_tokens", "abc"),
            Err("option is not live-updateable")
        );
        for (v, want) in [
            ("[SimpleStrategy]", vec!["SimpleStrategy"]),
            ("[\"SimpleStrategy\"]", vec!["SimpleStrategy"]),
            ("SimpleStrategy,EverywhereStrategy", vec!["SimpleStrategy", "EverywhereStrategy"]),
            ("", vec![]),
            ("[]", vec![]),
        ] {
            set_value(&mut s, "replication_strategy_warn_list", v).unwrap();
            assert_eq!(s.scylla.replication_strategy_warn_list, want, "{v}");
        }
        assert_eq!(set_value(&mut s, "replication_strategy_warn_list", "[Foo]"), Err(PARSE));
        set_value(&mut s, "write_consistency_levels_disallowed", "ONE, ANY").unwrap();
        assert_eq!(
            s.scylla.write_consistency_levels_disallowed,
            [Consistency::One, Consistency::Any]
        );
        assert_eq!(
            set_value(&mut s, "write_consistency_levels_disallowed", "INVALID_CL, ANY"),
            Err(PARSE)
        );
        for (v, want) in [("true", true), ("0", false), ("1", true), ("false", false)] {
            set_value(&mut s, "compaction_enforce_min_threshold", v).unwrap();
            assert_eq!(s.scylla.values["compaction_enforce_min_threshold"], want.to_string());
        }
        assert_eq!(set_value(&mut s, "compaction_enforce_min_threshold", "maybe"), Err(PARSE));
        set_value(&mut s, "restrict_replication_simplestrategy", "\"0\"").unwrap();
        assert_eq!(s.scylla.restrict_replication_simplestrategy, "0");
        set_value(&mut s, "restrict_replication_simplestrategy", "warn").unwrap();
        set_value(&mut s, "vector_store_secondary_uri", "\"\"").unwrap();
        assert_eq!(s.scylla.values["vector_store_secondary_uri"], "\"\"");
        set_value(&mut s, "vector_store_primary_uri", "http://x:1").unwrap();
        set_value(&mut s, "components_memory_reclaim_threshold", "1").unwrap();
        let mut o = BTreeMap::new();
        values(&s, &mut o);
        assert_eq!(o["replication_strategy_warn_list"], "[]");
        assert_eq!(o["write_consistency_levels_disallowed"], "[\"ONE\",\"ANY\"]");
        assert_eq!(o["restrict_replication_simplestrategy"], "\"warn\"");
        assert_eq!(o["vector_store_primary_uri"], "\"http://x:1\"");
        assert_eq!(o["components_memory_reclaim_threshold"], "1");
        assert_eq!(o["compaction_enforce_min_threshold"], "false");
        assert_eq!(source(&s, "query_tombstone_page_limit", "default"), "cql");
        assert_eq!(source(&s, "cluster_name", "default"), "default");
    }
}
