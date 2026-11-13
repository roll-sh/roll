//! Postgres column masks for a preview's branch (`masking` rules for `postgres:<ref>`): the
//! columns are rewritten in the branch, once it's made or reset and before anything serves it.
//! The parent is never touched. NoSQL masks are a read policy instead (`Brancher::mask`).

use std::collections::BTreeMap;

/// The rules a column can have.
pub const RULES: &str = "null, redact, hash, email or fixed:<value>";

/// What masking a branch runs: one statement that rewrites every masked column (one `UPDATE`
/// per table, all in one transaction, with the tables' triggers off so none copies the old
/// values elsewhere), then each table's `VACUUM FULL` (the rows as they were leave the branch's
/// files), every materialized view refreshed (none keeps the old values), and a checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub update: String,
    pub after: Vec<String>,
}

/// The plan for `columns` (`schema.table.column` → rule), or why a rule can't be one. `salt`
/// (random per run) keys `hash` and `email`, so a value can't be found again by hashing guesses;
/// within one run the same input still gives the same output, so joins on it match.
pub fn postgres(columns: &BTreeMap<String, String>, salt: &str) -> Result<Plan, String> {
    let salt = literal(salt);
    let mut tables: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (path, rule) in columns {
        let parts: Vec<&str> = path.split('.').collect();
        let [schema, table, column] = parts[..] else { return Err(format!("{path:?} isn't schema.table.column")) };
        if [schema, table, column].iter().any(|p| p.is_empty()) {
            return Err(format!("{path:?} isn't schema.table.column"));
        }
        let c = ident(column);
        let value = match rule.trim() {
            "null" => "NULL".to_string(),
            "redact" => format!("CASE WHEN {c} IS NULL THEN NULL ELSE '***' END"),
            "hash" => format!("md5({salt} || {c}::text)"),
            "email" => format!("CASE WHEN {c} IS NULL THEN NULL ELSE 'user-' || left(md5({salt} || {c}::text), 12) || '@example.invalid' END"),
            r => match r.strip_prefix("fixed:") {
                Some(v) => literal(v),
                None => return Err(format!("{path}: {rule:?} isn't a rule ({RULES})")),
            },
        };
        tables.entry((schema.to_string(), table.to_string())).or_default().push(format!("{c} = {value}"));
    }
    // Only the catalog's own names resolve (nothing the branch's role made can stand in for a
    // function this runs as the superuser), literals read as written, and no trigger of the masked
    // tables runs for the rewrite (an audit log's copy of OLD, a notification): each table's user
    // triggers are switched off and then put back exactly as they were (`ALWAYS`, `REPLICA` and
    // disabled ones too; `session_replication_role` would let those fire).
    let mut update = String::from("BEGIN;\nSET LOCAL search_path = pg_catalog, pg_temp;\nSET LOCAL standard_conforming_strings = on;\n");
    let rels: Vec<String> = tables.keys().map(|(schema, table)| format!("{}::regclass", literal(&format!("{}.{}", ident(schema), ident(table))))).collect();
    update.push_str(&format!(
        "CREATE TEMP TABLE roll_triggers ON COMMIT DROP AS SELECT tgrelid::regclass::text AS rel, tgname, tgenabled FROM pg_catalog.pg_trigger WHERE NOT tgisinternal AND tgrelid IN ({});\n",
        rels.join(", ")
    ));
    update.push_str("DO $roll$ DECLARE r record; BEGIN FOR r IN SELECT DISTINCT rel FROM roll_triggers LOOP EXECUTE pg_catalog.format('ALTER TABLE %s DISABLE TRIGGER USER', r.rel); END LOOP; END $roll$;\n");
    let mut after = Vec::new();
    for ((schema, table), sets) in &tables {
        let t = format!("{}.{}", ident(schema), ident(table));
        update.push_str(&format!("UPDATE {t} SET {};\n", sets.join(", ")));
        // Maintenance runs as the table's owner, with a safe search_path (PostgreSQL 17+).
        after.push(format!("VACUUM (FULL, ANALYZE) {t}"));
    }
    update.push_str(
        "DO $roll$ DECLARE r record; BEGIN FOR r IN SELECT * FROM roll_triggers LOOP EXECUTE pg_catalog.format('ALTER TABLE %s %s TRIGGER %I', r.rel, \
         CASE r.tgenabled WHEN 'A' THEN 'ENABLE ALWAYS' WHEN 'R' THEN 'ENABLE REPLICA' WHEN 'D' THEN 'DISABLE' ELSE 'ENABLE' END, r.tgname); END LOOP; END $roll$;\n",
    );
    update.push_str("COMMIT;");
    // A materialized view refreshes as its owner (the branch's role), never as the superuser.
    after.push(
        "SET search_path = pg_catalog, pg_temp; DO $roll$ DECLARE v record; BEGIN FOR v IN SELECT schemaname, matviewname FROM pg_catalog.pg_matviews WHERE ispopulated LOOP \
         EXECUTE pg_catalog.format('REFRESH MATERIALIZED VIEW %I.%I', v.schemaname, v.matviewname); END LOOP; END $roll$"
            .into(),
    );
    after.push("CHECKPOINT".into());
    Ok(Plan { update, after })
}

/// Whether `rule` is one a Postgres column can have.
pub fn is_rule(rule: &str) -> bool {
    matches!(rule.trim(), "null" | "redact" | "hash" | "email") || rule.trim().starts_with("fixed:")
}

/// A quoted identifier: any name, as itself.
fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A quoted string literal (standard strings: only `'` needs doubling).
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    /// Rules become one transaction per branch, names and values quoted whatever they hold.
    #[test]
    fn rules_become_one_quoted_transaction() {
        let cols: BTreeMap<String, String> = [
            ("public.users.email", "email"),
            ("public.users.name", "fixed:it's; DROP TABLE users; --"),
            ("app.we\"ird.ip", "null"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let p = super::postgres(&cols, "s4lt").unwrap();
        assert!(p.update.starts_with("BEGIN;\n") && p.update.ends_with("COMMIT;"), "{}", p.update);
        assert!(p.update.contains("SET LOCAL search_path = pg_catalog, pg_temp"), "only the catalog's names: {}", p.update);
        assert!(p.update.contains("DISABLE TRIGGER USER") && p.update.contains("ENABLE ALWAYS"), "triggers off, then back as they were: {}", p.update);
        assert!(!p.update.contains("session_replication_role"), "REPLICA triggers would fire: {}", p.update);
        assert!(p.update.contains("md5('s4lt' || \"email\"::text)"), "salted: {}", p.update);
        assert!(p.update.contains(r#"UPDATE "public"."users" SET "email" = CASE WHEN "email" IS NULL"#), "{}", p.update);
        assert!(p.update.contains(r#""name" = 'it''s; DROP TABLE users; --'"#), "a value stays one literal: {}", p.update);
        assert!(p.update.contains(r#"UPDATE "app"."we""ird" SET "ip" = NULL"#), "a quote in a name stays a name: {}", p.update);
        assert_eq!(p.after.len(), 4, "two tables' vacuums, the views, a checkpoint");
        assert!(super::postgres(&[("a.b.c".to_string(), "mask_default()".to_string())].into_iter().collect(), "s").is_err());
    }
}
