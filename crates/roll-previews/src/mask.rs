//! Postgres column masks for a preview's branch (`masking` rules for `postgres:<ref>`): the
//! columns are rewritten in the branch, once it's made or reset and before anything serves it.
//! The parent is never touched. NoSQL masks are a read policy instead (`Brancher::mask`), which
//! can't hide a key column ([`nosql_key_masks`]).

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

/// The NoSQL masks (`keyspace.table.column`) that name a column of a primary key, in the
/// parent's schema (roll nosql's `/schema`): the table, or a view of it keyed by that column,
/// can't be read with it masked (a key reaches its reader however the rows are masked), so the
/// preview's pages would fail. One line per problem; columns the schema doesn't have yet (a
/// migration may add them) are none.
pub fn nosql_key_masks(schema: &serde_json::Value, columns: &BTreeMap<String, String>) -> Vec<String> {
    let has = |t: &serde_json::Value, column: &str| {
        ["partition_key", "clustering_key"].iter().any(|k| t[k].as_array().is_some_and(|a| a.iter().any(|c| c == column)))
    };
    let mut out = Vec::new();
    for path in columns.keys() {
        let mut parts = path.splitn(3, '.');
        let (Some(ks), Some(table), Some(column)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some(k) = schema["keyspaces"].as_array().into_iter().flatten().find(|k| k["name"] == ks) else { continue };
        let tables = k["tables"].as_array().into_iter().flatten();
        if tables.filter(|t| t["name"] == table).any(|t| has(t, column)) {
            out.push(format!("{path} is in {ks}.{table}'s primary key, and a table can't be read with a key column masked: mask its other columns"));
            continue;
        }
        for v in k["views"].as_array().into_iter().flatten().filter(|v| v["base_table"] == table && has(v, column)) {
            out.push(format!("{path} is in the key of the view {ks}.{}, which can't be read with it masked: mask other columns, or drop the view from what the preview reads", v["name"].as_str().unwrap_or_default()));
        }
    }
    out
}

/// Search field masks that can't work against a `_mapping` answer (index → `mappings`): `redact`
/// and `email` write a string, which only a text-like field takes (a `dense_vector`, a number, a
/// date would refuse it and fail the masking, and the preview with it); such a field can only be
/// `remove`d. One line per problem; a field the mapping doesn't have yet (a document may add it)
/// is none.
pub fn search_field_masks(mapping: &serde_json::Value, fields: &BTreeMap<String, String>) -> Vec<String> {
    // The types a string goes into (`constant_keyword` takes only its one value, so not it).
    const TEXT: [&str; 8] = ["text", "keyword", "match_only_text", "wildcard", "search_as_you_type", "semantic_text", "completion", "token_count"];
    let mut out = Vec::new();
    for (spec, rule) in fields {
        if rule == "remove" {
            continue;
        }
        let Some((index, path)) = spec.split_once('.') else { continue };
        // The index as named, or every index an alias or pattern answered for (`GET /<it>/_mapping`
        // is keyed by the real names): the first that says the field isn't text.
        let named: Vec<&serde_json::Value> = match mapping.get(index) {
            Some(m) => vec![m],
            None => mapping.as_object().into_iter().flatten().map(|(_, m)| m).collect(),
        };
        let Some(props) = named.iter().map(|m| &m["mappings"]["properties"]).find(|props| field_type(props, path).is_some_and(|t| !TEXT.contains(&t.as_str()) && t != "?")).or_else(|| named.first().map(|m| &m["mappings"]["properties"])) else { continue };
        if let Some(t) = field_type(props, path).filter(|t| !TEXT.contains(&t.as_str()) && t != "?") {
            let a = if t.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
            out.push(format!("{spec} is {a} {t} field: {rule} writes a string it won't take; use remove"));
        }
    }
    out
}

/// A field's mapping type down `path` through objects' `properties` (nested ones too), if the
/// mapping has it.
fn field_type(props: &serde_json::Value, path: &str) -> Option<String> {
    let mut at = props;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, part) in parts.iter().enumerate() {
        let f = at.get(*part)?;
        if i == parts.len() - 1 {
            return Some(f["type"].as_str().unwrap_or(if f.get("properties").is_some() { "object" } else { "?" }).to_string());
        }
        at = &f["properties"];
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_nosql_mask_on_a_key_column_is_named() {
        let schema = serde_json::json!({ "keyspaces": [{ "name": "app",
            "tables": [{ "name": "users", "partition_key": ["id"], "clustering_key": [] }],
            "views": [{ "name": "users_by_email", "base_table": "users", "partition_key": ["email"], "clustering_key": ["id"] }],
        }]});
        let masks = |cols: &[&str]| cols.iter().map(|c| (c.to_string(), "mask_default()".to_string())).collect();
        let got = super::nosql_key_masks(&schema, &masks(&["app.users.id", "app.users.email", "app.users.name", "app.later.x", "other.t.c"]));
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got[0].starts_with("app.users.email is in the key of the view app.users_by_email"), "{got:?}");
        assert!(got[1].starts_with("app.users.id is in app.users's primary key"), "{got:?}");
    }

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

    /// A string mask on a vector, a number or an object is named with what it is; on text, or
    /// `remove` on anything, or a field the mapping doesn't have, it isn't.
    #[test]
    fn search_masks_only_write_strings_into_text() {
        let mapping = serde_json::json!({ "products": { "mappings": { "properties": {
            "name": { "type": "text" }, "sku": { "type": "keyword" }, "emb": { "type": "dense_vector", "dims": 3 },
            "price": { "type": "float" }, "owner": { "properties": { "email": { "type": "keyword" }, "age": { "type": "integer" } } },
        } } } });
        let rules: BTreeMap<String, String> = [
            ("products.name", "redact"), ("products.sku", "email"), ("products.emb", "redact"), ("products.price", "remove"),
            ("products.owner.email", "email"), ("products.owner.age", "redact"), ("products.owner", "redact"), ("products.later", "redact"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(
            super::search_field_masks(&mapping, &rules),
            [
                "products.emb is a dense_vector field: redact writes a string it won't take; use remove",
                "products.owner is an object field: redact writes a string it won't take; use remove",
                "products.owner.age is an integer field: redact writes a string it won't take; use remove",
            ]
        );
        // An alias answers with its real index's name: that one is checked.
        let aliased = serde_json::json!({ "products-v2": { "mappings": { "properties": { "emb": { "type": "dense_vector" } } } } });
        let rule: BTreeMap<String, String> = [("products.emb".to_string(), "redact".to_string())].into_iter().collect();
        assert_eq!(super::search_field_masks(&aliased, &rule).len(), 1, "through the alias");
    }
}
