//! ScyllaDB's write consistency level guardrails: every INSERT, UPDATE, DELETE and BATCH
//! (conditional or not) is counted per consistency level
//! (`scylla_cql_writes_per_consistency_level`); a level on `write_consistency_levels_disallowed`
//! fails the write with an InvalidRequest, one on `write_consistency_levels_warned` adds a
//! warning. As in ScyllaDB, the guardrails apply to every role, superusers included.

use crate::database::Database;
use crate::error::{Consistency, Error, Result};
use crate::guardrails::Metrics;

/// The warning for a write at a level of `write_consistency_levels_warned`.
pub fn warned_message(cl: Consistency) -> String {
    format!(
        "Write consistency level {} is not recommended: it is listed in write_consistency_levels_warned. It is not recommended to use it for writes.",
        cl.name()
    )
}

/// The error for a write at a level of `write_consistency_levels_disallowed`.
pub fn disallowed_message(cl: Consistency) -> String {
    format!(
        "Write consistency level {} is forbidden by the current configuration setting of write_consistency_levels_disallowed. Please use a different consistency level, or remove {} from write_consistency_levels_disallowed set in the configuration.",
        cl.name(),
        cl.name()
    )
}

/// Checks a write at `cl` (ScyllaDB dialect); returns the warnings to send.
pub fn check(db: &Database, cl: Consistency) -> Result<Vec<String>> {
    if !db.dialect().is_scylla() {
        return Ok(Vec::new());
    }
    let m = db.guardrail_metrics();
    m.count_write(cl);
    let s = db.settings();
    if s.scylla.write_consistency_levels_disallowed.contains(&cl) {
        Metrics::inc(&m.write_consistency_levels_disallowed_violations);
        return Err(Error::invalid(disallowed_message(cl)));
    }
    if s.scylla.write_consistency_levels_warned.contains(&cl) {
        Metrics::inc(&m.write_consistency_levels_warned_violations);
        return Ok(vec![warned_message(cl)]);
    }
    Ok(Vec::new())
}
