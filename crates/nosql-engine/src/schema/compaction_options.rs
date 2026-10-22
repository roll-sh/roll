//! The `compaction = {…}` table option: the strategy class, its sub-options and their
//! validation per dialect.
//!
//! Cassandra's checks are ported from `CompactionParams.create` / `validate`,
//! `AbstractCompactionStrategy.validateOptions`, `SizeTieredCompactionStrategyOptions`,
//! `LeveledCompactionStrategy.validateOptions` and `TimeWindowCompactionStrategyOptions`
//! (Apache-2.0): `provide_overlapping_tombstones`, the common options, the strategy's own, the
//! unknown ones ("Properties specified [...] are not understood by ..."), then the thresholds.
//! ScyllaDB's (texts from the reference) check the common options, the strategy's, then the
//! thresholds, and report unknown options last ("Invalid compaction strategy options {...} for
//! chosen strategy type"); an unparsable number is a syntax error there.

use std::collections::BTreeMap;

use nosql_types::Dialect;

use crate::error::{Error, Result};

const CASSANDRA_PKG: &str = "org.apache.cassandra.db.compaction.";

/// The options a strategy understands besides `class` (validation aside).
fn known(d: Dialect, short: &str) -> Option<Vec<&'static str>> {
    const SCYLLA_COMMON: &[&str] = &[
        "tombstone_threshold",
        "tombstone_compaction_interval",
        "unchecked_tombstone_compaction",
        "enabled",
    ];
    const CASSANDRA_COMMON: &[&str] = &[
        "tombstone_threshold",
        "tombstone_compaction_interval",
        "unchecked_tombstone_compaction",
        "log_all",
        "enabled",
        "only_purge_repaired_tombstones",
        "provide_overlapping_tombstones",
    ];
    let own: &[&str] = match (d, short) {
        (Dialect::Scylla, "SizeTieredCompactionStrategy") => &[
            "min_threshold",
            "max_threshold",
            "min_sstable_size",
            "bucket_low",
            "bucket_high",
            "cold_reads_to_omit",
        ],
        (Dialect::Scylla, "LeveledCompactionStrategy") => &[
            "sstable_size_in_mb",
            "min_threshold",
            "max_threshold",
            "min_sstable_size",
            "bucket_low",
            "bucket_high",
            "cold_reads_to_omit",
        ],
        (Dialect::Scylla, "TimeWindowCompactionStrategy") => &[
            "compaction_window_unit",
            "compaction_window_size",
            "timestamp_resolution",
            "enable_optimized_twcs_queries",
            "expired_sstable_check_frequency_seconds",
            "unsafe_aggressive_sstable_expiration",
            "min_threshold",
            "max_threshold",
            "min_sstable_size",
            "bucket_low",
            "bucket_high",
            "cold_reads_to_omit",
        ],
        (Dialect::Scylla, "IncrementalCompactionStrategy") => &[
            "sstable_size_in_mb",
            "space_amplification_goal",
            "min_threshold",
            "max_threshold",
            "min_sstable_size",
            "bucket_low",
            "bucket_high",
        ],
        (Dialect::Scylla, "NullCompactionStrategy") => &[],
        (Dialect::Cassandra, "SizeTieredCompactionStrategy") => &[
            "min_threshold",
            "max_threshold",
            "min_sstable_size",
            "bucket_low",
            "bucket_high",
        ],
        (Dialect::Cassandra, "LeveledCompactionStrategy") => &[
            "sstable_size_in_mb",
            "fanout_size",
            "single_sstable_uplevel",
            "min_threshold",
            "max_threshold",
            "min_sstable_size",
            "bucket_low",
            "bucket_high",
        ],
        (Dialect::Cassandra, "TimeWindowCompactionStrategy") => &[
            "compaction_window_size",
            "compaction_window_unit",
            "timestamp_resolution",
            "expired_sstable_check_frequency_seconds",
            "unsafe_aggressive_sstable_expiration",
            "min_threshold",
            "max_threshold",
            "min_sstable_size",
            "bucket_low",
            "bucket_high",
        ],
        (Dialect::Cassandra, "UnifiedCompactionStrategy") => &[
            "scaling_parameters",
            "base_shard_count",
            "target_sstable_size",
            "flush_size_override",
            "max_sstables_to_compact",
            "expired_sstable_check_frequency_seconds",
            "unsafe_aggressive_sstable_expiration",
            "overlap_inclusion_method",
            "min_sstable_size",
            "sstable_growth",
        ],
        _ => return None,
    };
    let common = if d.is_scylla() { SCYLLA_COMMON } else { CASSANDRA_COMMON };
    Some(common.iter().chain(own).copied().collect())
}

/// ScyllaDB prints doubles the shortest way (`0`, `-0.4`, `1.3`).
fn shortest(v: f64) -> String {
    format!("{v}")
}

/// Java's `Double.toString` (`1.5`, `5.0`).
fn java_double(v: f64) -> String {
    nosql_types::float::java_double(v)
}

/// Validates and normalises `compaction = {…}`.
pub fn compaction(d: Dialect, mut m: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let Some(class) = m.remove("class") else {
        return Err(Error::config(match d {
            Dialect::Scylla => "Missing sub-option 'class' for the 'compaction' option.",
            Dialect::Cassandra => "Missing sub-option 'class' for the 'compaction' option",
        }));
    };
    let short = class.rsplit('.').next().unwrap_or(&class).to_string();
    // Cassandra resolves classes by name: the package must be right when given.
    let package_ok =
        !class.contains('.') || class == format!("{CASSANDRA_PKG}{short}") || d.is_scylla();
    let Some(known) = known(d, &short).filter(|_| package_ok) else {
        return Err(Error::config(match d {
            Dialect::Scylla => format!("Unable to find compaction strategy class '{class}'"),
            Dialect::Cassandra => {
                let full = if class.contains('.') {
                    class.clone()
                } else {
                    format!("{CASSANDRA_PKG}{class}")
                };
                format!("Unable to find compaction strategy class '{full}'")
            }
        }));
    };
    match d {
        Dialect::Scylla => scylla(&short, &m, &known)?,
        Dialect::Cassandra => cassandra(&short, &m, &known)?,
    }
    let mut out = m;
    match d {
        Dialect::Scylla => {
            out.remove("min_threshold");
            out.remove("max_threshold");
            out.insert("class".into(), short);
        }
        Dialect::Cassandra => {
            if short != "UnifiedCompactionStrategy" {
                out.entry("min_threshold".into())
                    .or_insert_with(|| "4".into());
                out.entry("max_threshold".into())
                    .or_insert_with(|| "32".into());
            }
            out.insert("class".into(), format!("{CASSANDRA_PKG}{short}"));
        }
    }
    Ok(out)
}

// ---- ScyllaDB ----------------------------------------------------------------------------------

fn scylla(short: &str, m: &BTreeMap<String, String>, known: &[&str]) -> Result<()> {
    let bad = |msg: String| Error::config(msg);
    let double = |k: &str| -> Result<Option<f64>> {
        m.get(k)
            .map(|v| {
                v.trim()
                    .parse::<f64>()
                    .map_err(|_| Error::syntax(format!("Invalid double value {v} for '{k}'")))
            })
            .transpose()
    };
    let long = |k: &str, quoted: bool| -> Result<Option<i64>> {
        m.get(k)
            .map(|v| {
                v.trim().parse::<i64>().map_err(|_| {
                    Error::syntax(if quoted {
                        format!("Invalid long value {v} for '{k}'")
                    } else {
                        format!("Invalid long value {v} for {k}")
                    })
                })
            })
            .transpose()
    };
    let boolean = |k: &str| -> Result<()> {
        match m.get(k) {
            Some(v) if v != "true" && v != "false" => Err(bad(format!(
                "{k} value ({v}) must be \"true\" or \"false\""
            ))),
            _ => Ok(()),
        }
    };
    // Common options.
    if let Some(v) = double("tombstone_threshold")?
        && !(0.0..=1.0).contains(&v)
    {
        return Err(bad(format!(
            "tombstone_threshold value ({}) must be between 0.0 and 1.0",
            shortest(v)
        )));
    }
    if let Some(v) = long("tombstone_compaction_interval", true)?
        && v <= 0
    {
        return Err(bad(format!(
            "tombstone_compaction_interval value ({}ms) must be positive",
            v.saturating_mul(1000)
        )));
    }
    boolean("unchecked_tombstone_compaction")?;
    boolean("enabled")?;
    // The strategy's.
    let stcs_like = matches!(
        short,
        "SizeTieredCompactionStrategy"
            | "LeveledCompactionStrategy"
            | "TimeWindowCompactionStrategy"
            | "IncrementalCompactionStrategy"
    );
    if short == "LeveledCompactionStrategy"
        && let Some(v) = m.get("sstable_size_in_mb")
    {
        let n: i32 = v.trim().parse().map_err(|_| {
            Error::syntax(format!("Invalid integer value {v} for 'sstable_size_in_mb'"))
        })?;
        if n <= 0 {
            return Err(bad(format!("sstable_size_in_mb value ({n}) must be positive")));
        }
    }
    if short == "TimeWindowCompactionStrategy" {
        if let Some(u) = m.get("compaction_window_unit")
            && !matches!(u.as_str(), "MINUTES" | "HOURS" | "DAYS")
        {
            return Err(bad(format!("Invalid window unit {u} for compaction_window_unit")));
        }
        if let Some(n) = long("compaction_window_size", true)?
            && n < 1
        {
            return Err(bad(format!("compaction_window_size value ({n}) must be greater than 1")));
        }
        if let Some(r) = m.get("timestamp_resolution")
            && !matches!(r.as_str(), "MICROSECONDS" | "MILLISECONDS")
        {
            return Err(bad(format!(
                "Invalid timestamp resolution {r} for timestamp_resolution"
            )));
        }
        boolean("enable_optimized_twcs_queries")?;
        long("expired_sstable_check_frequency_seconds", false)?;
    }
    if stcs_like {
        if let Some(n) = long("min_sstable_size", true)?
            && n < 0
        {
            return Err(bad(format!("min_sstable_size value ({n}) must be non negative")));
        }
        if let Some(v) = double("bucket_low")?
            && !(v > 0.0 && v < 1.0)
        {
            return Err(bad(format!(
                "bucket_low value ({}) must be between 0.0 and 1.0",
                shortest(v)
            )));
        }
        if let Some(v) = double("bucket_high")?
            && v <= 1.0
        {
            return Err(bad(format!(
                "bucket_high value ({}) must be greater than 1.0",
                shortest(v)
            )));
        }
        if short != "IncrementalCompactionStrategy"
            && let Some(v) = double("cold_reads_to_omit")?
            && !(0.0..=1.0).contains(&v)
        {
            return Err(bad(format!(
                "cold_reads_to_omit value ({}) must be between 0.0 and 1.0",
                shortest(v)
            )));
        }
    }
    if short == "IncrementalCompactionStrategy"
        && let Some(v) = double("space_amplification_goal")?
        && !(v > 1.0 && v <= 2.0)
    {
        return Err(bad(format!(
            "space_amplification_goal value ({}) must be greater than 1.0 and less than or equal to 2.0",
            shortest(v)
        )));
    }
    if stcs_like {
        for k in ["min_threshold", "max_threshold"] {
            if let Some(n) = long(k, true)?
                && n < 2
            {
                return Err(bad(format!("{k} value ({n}) must be bigger or equal to 2")));
            }
        }
    }
    let unknown: Vec<String> = m
        .iter()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, v)| format!("\"{k}\": \"{v}\""))
        .collect();
    if !unknown.is_empty() {
        return Err(bad(format!(
            "Invalid compaction strategy options {{{}}} for chosen strategy type",
            unknown.join(", ")
        )));
    }
    Ok(())
}

// ---- Cassandra ---------------------------------------------------------------------------------

/// Java's `%f`.
fn java_f(v: f64) -> String {
    format!("{v:.6}")
}

fn cassandra(short: &str, m: &BTreeMap<String, String>, known: &[&str]) -> Result<()> {
    let bad = |msg: String| Error::config(msg);
    let not_int = |v: &str, k: &str| bad(format!("{v} is not a parsable int (base10) for {k}"));
    let int = |k: &str| -> Result<Option<i64>> {
        m.get(k)
            .map(|v| v.trim().parse::<i32>().map(i64::from).map_err(|_| not_int(v, k)))
            .transpose()
    };
    let long = |k: &str| -> Result<Option<i64>> {
        m.get(k)
            .map(|v| v.trim().parse::<i64>().map_err(|_| not_int(v, k)))
            .transpose()
    };
    // `CompactionParams.create`.
    if let Some(v) = m.get("provide_overlapping_tombstones") {
        let up = v.to_uppercase();
        if !matches!(up.as_str(), "NONE" | "ROW" | "CELL") {
            return Err(bad(format!(
                "Invalid value {up} for 'provide_overlapping_tombstones' compaction sub-option - must be one of the following [NONE, ROW, CELL]."
            )));
        }
    }
    if short != "UnifiedCompactionStrategy" {
        // `AbstractCompactionStrategy.validateOptions`.
        if let Some(v) = m.get("tombstone_threshold") {
            let f: f64 = v
                .trim()
                .parse::<f32>()
                .map(f64::from)
                .map_err(|_| not_int(v, "tombstone_threshold"))?;
            if f < 0.0 {
                return Err(bad(format!(
                    "tombstone_threshold must be greater than 0, but was {}",
                    java_f(f)
                )));
            }
        }
        if let Some(n) = long("tombstone_compaction_interval")?
            && n < 0
        {
            return Err(bad(format!(
                "tombstone_compaction_interval must be greater than 0, but was {n}"
            )));
        }
        if let Some(v) = m.get("unchecked_tombstone_compaction")
            && !v.eq_ignore_ascii_case("true")
            && !v.eq_ignore_ascii_case("false")
        {
            return Err(bad(format!(
                "'unchecked_tombstone_compaction' should be either 'true' or 'false', not '{v}'"
            )));
        }
        if let Some(v) = m.get("log_all")
            && !v.eq_ignore_ascii_case("true")
            && !v.eq_ignore_ascii_case("false")
        {
            return Err(bad(format!(
                "'log_all' should either be 'true' or 'false', not {v}"
            )));
        }
        if let Some(v) = m.get("enabled")
            && !v.eq_ignore_ascii_case("true")
            && !v.eq_ignore_ascii_case("false")
        {
            return Err(bad(format!(
                "enabled should either be 'true' or 'false', not {v}"
            )));
        }
    }
    if short == "LeveledCompactionStrategy" {
        if let Some(n) = int("sstable_size_in_mb")?
            && n < 1
        {
            return Err(bad(format!("sstable_size_in_mb must be larger than 0, but was {n}")));
        }
        if let Some(n) = int("fanout_size")?
            && n < 1
        {
            return Err(bad(format!("fanout_size must be larger than 0, but was {n}")));
        }
    }
    if short == "TimeWindowCompactionStrategy" {
        if let Some(r) = m.get("timestamp_resolution")
            && !matches!(r.as_str(), "SECONDS" | "MILLISECONDS" | "MICROSECONDS" | "NANOSECONDS")
        {
            return Err(bad(format!("{r} is not valid for timestamp_resolution")));
        }
        if let Some(u) = m.get("compaction_window_unit")
            && !matches!(u.as_str(), "MINUTES" | "HOURS" | "DAYS")
        {
            return Err(bad(format!("{u} is not valid for compaction_window_unit")));
        }
        if let Some(n) = int("compaction_window_size")?
            && n < 1
        {
            return Err(bad(format!("{n} must be greater than 1 for compaction_window_size")));
        }
        if let Some(n) = long("expired_sstable_check_frequency_seconds")?
            && n < 0
        {
            return Err(bad(format!(
                "expired_sstable_check_frequency_seconds must not be negative, but was {n}"
            )));
        }
        if let Some(v) = m.get("unsafe_aggressive_sstable_expiration")
            && !v.eq_ignore_ascii_case("true")
            && !v.eq_ignore_ascii_case("false")
        {
            return Err(bad(format!(
                "unsafe_aggressive_sstable_expiration is not 'true' or 'false' ({v})"
            )));
        }
    }
    if matches!(
        short,
        "SizeTieredCompactionStrategy" | "LeveledCompactionStrategy" | "TimeWindowCompactionStrategy"
    ) {
        // `SizeTieredCompactionStrategyOptions.validateOptions`.
        if let Some(n) = long("min_sstable_size")?
            && n < 0
        {
            return Err(bad(format!("min_sstable_size must be non negative: {n}")));
        }
        let float = |k: &str, default: f64| -> Result<f64> {
            m.get(k).map_or(Ok(default), |v| {
                v.trim()
                    .parse::<f64>()
                    .map_err(|_| bad(format!("{v} is not a parsable float for {k}")))
            })
        };
        let low = float("bucket_low", 0.5)?;
        let high = float("bucket_high", 1.5)?;
        if high <= low {
            return Err(bad(format!(
                "bucket_high value ({}) is less than or equal to the bucket_low value ({})",
                java_double(high),
                java_double(low)
            )));
        }
    }
    let unknown: Vec<&str> = m
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k))
        .collect();
    if !unknown.is_empty() {
        return Err(bad(format!(
            "Properties specified [{}] are not understood by {short}",
            unknown.join(", ")
        )));
    }
    // `CompactionParams.validate`: the thresholds.
    if short == "UnifiedCompactionStrategy" {
        return Ok(());
    }
    let threshold = |k: &str, default: i64| -> Result<i64> {
        match m.get(k) {
            None => Ok(default),
            Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
                v.parse().map_err(|_| not_int(v, k))
            }
            Some(v) => Err(bad(format!(
                "Invalid value {v} for '{k}' compaction sub-option - must be an integer"
            ))),
        }
    };
    let min = threshold("min_threshold", 4)?;
    let max = threshold("max_threshold", 32)?;
    if min <= 0 || max <= 0 {
        return Err(bad("Disabling compaction by setting compaction thresholds to 0 has been removed, set the compaction option 'enabled' to false instead.".into()));
    }
    if min <= 1 {
        return Err(bad(format!(
            "Min compaction threshold cannot be less than 2 (got {min})"
        )));
    }
    if min > max {
        return Err(bad(format!(
            "Min compaction threshold (got {min}) cannot be greater than max compaction threshold (got {max})"
        )));
    }
    Ok(())
}
