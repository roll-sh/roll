use crate::commands::{Command, INTERNAL, table};

fn check(c: &Command, full: &str, bad: &mut Vec<String>) {
    let Some(m) = super::get(full) else {
        bad.push(format!("{full}: Redis has no such command"));
        return;
    };
    if m.arity() != Some(c.arity as i64) {
        bad.push(format!("{full}: arity {} (redis {:?})", c.arity, m.arity()));
    }
    let mut ours: Vec<String> = c.flag_names().iter().map(|s| s.to_string()).collect();
    let mut theirs = m.flags();
    ours.sort();
    theirs.sort();
    if ours != theirs {
        bad.push(format!("{full}: flags {ours:?} (redis {theirs:?})"));
    }
    let keys = (c.first_key as i64, c.last_key as i64, c.key_step as i64);
    if m.keys() != Some(keys) {
        bad.push(format!("{full}: keys {keys:?} (redis {:?})", m.keys()));
    }
    let ours: Vec<String> = c.categories().iter().map(|s| format!("@{s}")).collect();
    if ours != m.categories() {
        bad.push(format!("{full}: categories {ours:?} (redis {:?})", m.categories()));
    }
}

/// Every command roll has reports the same arity, flags, key positions and ACL categories as
/// Redis 8.10.2.
#[test]
fn command_table_matches_redis() {
    let mut bad = Vec::new();
    for c in &table().all {
        // Internal commands (`_FT.*`) are hidden from COMMAND, so the capture lacks them.
        if c.has(INTERNAL) {
            continue;
        }
        check(c, c.name, &mut bad);
        for s in c.subs {
            check(s, &format!("{}|{}", c.name, s.name), &mut bad);
        }
    }
    assert!(bad.is_empty(), "{} differences:\n{}", bad.len(), bad.join("\n"));
}
