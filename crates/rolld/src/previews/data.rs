//! One branch adapter per data product, over this node's API.

use super::*;
use async_trait::async_trait;
use reqwest::Method;
use roll_previews::Kind;
use roll_previews::Mode;
use roll_previews::ports::{Branched, Brancher, Usage};
use serde_json::{Value, json};

// ── Postgres: a branch of the project's main ──

pub struct Postgres(pub Api);

impl Postgres {
    /// A branch of `parent` named `name` (`at`: as the parent was then), with its own role; one
    /// made just before a crash is found and finished instead.
    async fn create(&self, parent: &str, name: &str, mode: Mode, at: Option<i64>) -> anyhow::Result<Branched> {
        self.create_from(parent, None, name, mode, at).await
    }

    /// [`Self::create`] from one of the project's branches (`from`, its id) instead of `main`.
    async fn create_from(&self, parent: &str, from: Option<&str>, name: &str, mode: Mode, at: Option<i64>) -> anyhow::Result<Branched> {
        let (id, has_role) = match self.find(parent, name).await? {
            Some(b) => {
                let id = s(&b["id"]).unwrap_or_default();
                // From before a crash: it may still hold rows, so empty it again (idempotent).
                if mode == Mode::Schema {
                    self.0.post(&format!("/v1/projects/{}/branches/{}/reset", seg(parent), seg(&id)), json!({ "schema_only": true })).await?;
                }
                (id, false)
            }
            None => match self.0.post(&format!("/v1/projects/{}/branches", seg(parent)), json!({ "name": name, "parent": from, "schema_only": mode == Mode::Schema, "app_role": true, "at": at })).await {
                Ok(v) => (s(&v["branch"]["id"]).ok_or_else(|| anyhow::anyhow!("a branch without an id"))?, v["app_connection_uri"].is_string()),
                Err(e) if conflict_is_ours(&e) => {
                    let b = self.find(parent, name).await?.ok_or(e)?;
                    (s(&b["id"]).unwrap_or_default(), false)
                }
                Err(e) => return Err(e),
            },
        };
        // A branch found again (a crash) gets its role now (idempotent).
        if !has_role {
            self.0.post(&format!("/v1/projects/{}/branches/{}/app-role", seg(parent), seg(&id)), json!({})).await?;
        }
        Ok(Branched { r#ref: id, kept: Some(APP_ROLE.into()) })
    }

    async fn find(&self, project: &str, name: &str) -> anyhow::Result<Option<Value>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(project))).await?;
        Ok(p["branches"].as_array().and_then(|b| b.iter().find(|b| b["name"] == name).cloned()))
    }
}

#[async_trait]
impl Brancher for Postgres {
    fn kind(&self) -> Kind {
        Kind::Postgres
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        match self.find(parent, name).await {
            Ok(b) => Ok(b.and_then(|b| s(&b["id"]))),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `Mode::Schema`: branched, then every user table emptied (`schema_only`). Either way the
    /// branch gets a role of its own for the preview's code (pg-control's `app_role`).
    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.create(parent, name, mode, None).await
    }

    /// A branch of the preview's own branch (pg-control's `parent`), with a role of its own: an
    /// earlier push's data, copy-on-write.
    async fn snapshot(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<Option<Branched>> {
        self.create_from(parent, Some(branch), name, Mode::Branch, None).await.map(Some)
    }

    fn restores(&self) -> bool {
        true
    }

    /// pg-control's reset from another branch (`from`: the restore point): same branch,
    /// endpoint, `DATABASE_URL` and role (made again, same password).
    async fn restore(&self, parent: &str, branch: &str, _kept: Option<&str>, point: &str, _name: &str, _mode: Mode) -> anyhow::Result<Option<Branched>> {
        self.0.post(&format!("/v1/projects/{}/branches/{}/reset", seg(parent), seg(branch)), json!({ "from": point })).await?;
        Ok(Some(Branched { r#ref: branch.to_string(), kept: Some(APP_ROLE.into()) }))
    }

    /// From the parent's history (pg-control's point in time: unix seconds).
    async fn branch_at(&self, parent: &str, name: &str, mode: Mode, at: i64) -> anyhow::Result<Option<Branched>> {
        self.create(parent, name, mode, Some(at)).await.map(Some)
    }

    /// A reset in place can't go back in time: branched again from there instead.
    async fn reset_at(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode, at: i64) -> anyhow::Result<Option<Branched>> {
        self.delete(parent, branch, name).await?;
        self.create(parent, name, mode, Some(at)).await.map(Some)
    }


    /// In place: same branch, endpoint and `DATABASE_URL` (pg-control makes the branch's role
    /// again, same password).
    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        match self.0.post(&format!("/v1/projects/{}/branches/{}/reset", seg(parent), seg(branch)), json!({ "schema_only": mode == Mode::Schema })).await {
            Ok(_) => Ok(Branched { r#ref: branch.to_string(), kept: Some(APP_ROLE.into()) }),
            // Gone (or an older node without the route): branch again.
            Err(e) if is_404(&e) => {
                self.delete(parent, branch, name).await?;
                self.branch(parent, name, mode).await
            }
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let p = match self.0.get(&format!("/v1/projects/{}", seg(parent))).await {
            Ok(p) => p,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some(b) = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()) else { return Ok(()) };
        anyhow::ensure!(b["name"] == name && !b["parent_id"].is_null(), "refusing to delete Postgres branch {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/projects/{}/branches/{}", seg(parent), seg(branch))).await
    }

    /// A branch from before branch roles: its role, made now.
    async fn credentials(&self, parent: &str, branch: &str, _name: &str) -> anyhow::Result<Option<String>> {
        self.0.post(&format!("/v1/projects/{}/branches/{}/app-role", seg(parent), seg(branch)), json!({})).await?;
        Ok(Some(APP_ROLE.into()))
    }

    /// The branch's endpoint, signed in as the branch's own role (never the project's superuser).
    async fn connect(&self, parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).ok_or_else(|| anyhow::anyhow!("no Postgres branch {branch}"))?;
        let uri = s(&b["app_connection_uri"]).ok_or_else(|| anyhow::anyhow!("Postgres branch {branch} has no role of its own yet"))?;
        Ok(vec![("DATABASE_URL".into(), uri)])
    }

    /// Evict the branch: its local data directory goes to object storage (a base backup); the
    /// next connection restores it. On ext4 this is what makes an idle Postgres branch cheap.
    async fn idle(&self, parent: &str, branch: &str) -> anyhow::Result<()> {
        match self.0.post(&format!("/v1/projects/{}/branches/{}/evict", seg(parent), seg(branch)), json!({})).await {
            Ok(_) => Ok(()),
            Err(e) if is_404(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Rewrite the masked columns in the branch, in one transaction, then `VACUUM FULL` those
    /// tables so the rows as they were leave the branch's files (`roll_previews::mask`). Run again
    /// it's harmless: masked values stay masked.
    async fn mask(&self, parent: &str, branch: &str, _kept: Option<&str>, columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("randomness: {e}"))?;
        let salt: String = salt.iter().map(|b| format!("{b:02x}")).collect();
        let plan = roll_previews::mask::postgres(columns, &salt).map_err(|e| anyhow::anyhow!(e))?;
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).ok_or_else(|| anyhow::anyhow!("no Postgres branch {branch}"))?;
        anyhow::ensure!(!b["parent_id"].is_null(), "refusing to mask {branch}: it isn't a branch of {parent}");
        let ep = s(&b["endpoint"]["id"]).ok_or_else(|| anyhow::anyhow!("Postgres branch {branch} has no endpoint"))?;
        let query = format!("/v1/endpoints/{}/query", seg(&ep));
        self.0.post(&query, json!({ "sql": plan.update })).await?;
        for v in &plan.after {
            self.0.post(&query, json!({ "sql": v })).await?;
        }
        Ok(())
    }

    /// The parent branch's schema and this one's, from the catalog of each endpoint's `main`
    /// database (`SCHEMA_SQL`). Reading the parent's starts its compute if it sleeps. The branch is
    /// read as its own role (its code could have planted functions a superuser's query would
    /// call), the parent as the superuser, both with only the catalog's names in reach.
    async fn schema(&self, parent: &str, branch: &str) -> anyhow::Result<Option<(Vec<String>, Vec<String>)>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let branches = p["branches"].as_array().cloned().unwrap_or_default();
        let b = branches.iter().find(|b| b["id"] == branch).ok_or_else(|| anyhow::anyhow!("no Postgres branch {branch}"))?;
        let from = branches.iter().find(|x| x["id"] == b["parent_id"]).ok_or_else(|| anyhow::anyhow!("Postgres branch {branch} has no parent"))?;
        // pg-control names a branch's role after the branch.
        let role = b["app_connection_uri"].is_string().then(|| s(&b["id"])).flatten();
        Ok(Some((pg_schema_lines(&self.0, s(&from["endpoint"]["id"]), None).await?, pg_schema_lines(&self.0, s(&b["endpoint"]["id"]), role).await?)))
    }

    /// Its tables (as its own role sees them), or one's first rows, read-only, as its own role.
    async fn browse(&self, parent: &str, branch: &str, table: Option<&str>) -> anyhow::Result<Option<roll_previews::Browse>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).ok_or_else(|| anyhow::anyhow!("no Postgres branch {branch}"))?;
        anyhow::ensure!(!b["parent_id"].is_null(), "{branch} isn't a branch of {parent}");
        let ep = s(&b["endpoint"]["id"]).ok_or_else(|| anyhow::anyhow!("Postgres branch {branch} has no endpoint"))?;
        anyhow::ensure!(b["app_connection_uri"].is_string(), "Postgres branch {branch} has no role of its own yet");
        let role = s(&b["id"]).unwrap_or_default();
        let list = pg_read_as(&self.0, &ep, &role, BROWSE_TABLES_SQL).await?;
        let tables: Vec<roll_previews::BrowseTable> = list["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some(roll_previews::BrowseTable { name: r[0].as_str()?.to_string(), rows: r[1].as_str().and_then(|n| n.parse().ok()).unwrap_or(0).max(0) }))
            .collect();
        let Some(table) = table else { return Ok(Some(roll_previews::Browse::Tables { tables })) };
        // Only a table it listed: a name is quoted, never run.
        anyhow::ensure!(tables.iter().any(|t| t.name == table), "no table {table} the branch's role can read");
        let v = pg_read_as(&self.0, &ep, &role, &browse_rows_sql(table).ok_or_else(|| anyhow::anyhow!("{table} isn't schema.table"))?).await?;
        let cell = |c: &Value| c.as_str().map(|s| s.chars().take(200).collect::<String>());
        let mut rows: Vec<Vec<Option<String>>> = v["rows"].as_array().into_iter().flatten().map(|r| r.as_array().into_iter().flatten().map(cell).collect()).collect();
        let more = rows.len() > roll_previews::BROWSE_ROWS;
        rows.truncate(roll_previews::BROWSE_ROWS);
        let columns = v["columns"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(str::to_string)).collect();
        Ok(Some(roll_previews::Browse::Rows { table: table.to_string(), columns, rows, more }))
    }

    /// The branch's lines alone, as `schema` reads them (as its own role), the parent untouched.
    async fn branch_schema(&self, parent: &str, branch: &str) -> anyhow::Result<Option<Vec<String>>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).ok_or_else(|| anyhow::anyhow!("no Postgres branch {branch}"))?;
        let role = b["app_connection_uri"].is_string().then(|| s(&b["id"])).flatten();
        Ok(Some(pg_schema_lines(&self.0, s(&b["endpoint"]["id"]), role).await?))
    }

    /// The parent's table columns (`main`, the branch previews start from), as the superuser
    /// with only the catalog's names in reach, as `schema` reads it, for at most 10 s. A parent
    /// whose compute sleeps isn't woken for it ([`roll_previews::ports::Asleep`]).
    async fn columns(&self, parent: &str, max: usize) -> anyhow::Result<Vec<roll_previews::mask::Column>> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let branches = p["branches"].as_array().cloned().unwrap_or_default();
        let main = branches.iter().find(|b| b["name"] == "main").or_else(|| branches.iter().find(|b| b["parent_id"].is_null())).ok_or_else(|| anyhow::anyhow!("{parent} has no main branch"))?;
        let ep = s(&main["endpoint"]["id"]).ok_or_else(|| anyhow::anyhow!("{parent}'s main branch has no endpoint"))?;
        if main["endpoint"]["state"] != "running" {
            return Err(roll_previews::ports::Asleep.into());
        }
        let v = self.0.post(&format!("/v1/endpoints/{}/query", seg(&ep)), json!({ "sql": columns_sql(max) })).await?;
        let rows = v["results"].as_array().and_then(|r| r.last()).and_then(|r| r["rows"].as_array()).cloned().unwrap_or_default();
        Ok(rows.iter().filter_map(|r| column(r[0].as_str()?)).collect())
    }

    /// A branch's own bytes (its local data directory, else its objects), at Postgres's rates.
    async fn usage(&self, parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let p = self.0.get(&format!("/v1/projects/{}", seg(parent))).await?;
        let u = self.0.get(&format!("/v1/projects/{}/usage?days=1", seg(parent))).await?;
        let price = |k: &str, d: f64| u["pricing"][k].as_f64().unwrap_or(d);
        let b = p["branches"].as_array().and_then(|bs| bs.iter().find(|b| b["id"] == branch).cloned()).unwrap_or(Value::Null);
        let local = b["storage_bytes"].as_i64().unwrap_or(0);
        let objects = b["object_bytes"].as_i64().unwrap_or(0);
        let storage = gb(local) * price("usd_per_gb_month", 0.08) + gb(objects) * price("usd_per_gb_month_s3", 0.023);
        let cu = b["endpoint"]["min_cu"].as_f64().unwrap_or(0.25).max(0.25);
        Ok(Usage { bytes: local.max(objects), storage_usd_month: storage, active_usd_hour: cu * price("usd_per_cu_hour", 0.0408), month_to_date_usd: 0.0 })
    }
}

/// What a Postgres branch's `kept` says: it has a role of its own (pg-control's `app_role`).
const APP_ROLE: &str = r#"{"app_role":true}"#;

/// [`COLUMNS_SQL`] for at most `max` columns, with only the catalog's names in reach and a 10 s
/// statement timeout.
fn columns_sql(max: usize) -> String {
    format!("SET search_path = pg_catalog, pg_temp; SET statement_timeout = '10s'; {COLUMNS_SQL} LIMIT {max}")
}

/// One row of [`COLUMNS_SQL`].
fn column(row: &str) -> Option<roll_previews::mask::Column> {
    use roll_previews::mask::Takes;
    let c: Value = serde_json::from_str(row).ok()?;
    let takes = match c["takes"].as_str()? {
        "any" => Takes::Any,
        "null" => Takes::Null,
        _ => Takes::Nothing,
    };
    Some(roll_previews::mask::Column { path: s(&c["path"])?, r#type: s(&c["type"]), not_null: c["not_null"] == true, unique: c["unique"] == true, takes })
}

/// The user's tables' columns (a partitioned table's, not each partition's), one JSON object
/// each (`{path, type, not_null, unique, takes}`), for mask suggestions: what a mask may write
/// into each without failing (`roll_previews::mask::Takes`). A name with a dot in it can't be
/// named by a mask (`schema.table.column`), and a generated or `GENERATED ALWAYS` identity column
/// can't be written, so they're left out. What holds a column is read across its table's
/// partitions (by name):
/// - `none` (nothing): a foreign key other rows point at, a CHECK, an exclusion constraint, a
///   unique index's expression or predicate, a partition key, a domain with constraints;
/// - `null` (only a null): a foreign key's own column, an array, a range, an enum, a composite;
/// - `unique`: a unique index's column; `nnd`: one `NULLS NOT DISTINCT` (a null fails too).
const COLUMNS_SQL: &str = r"
WITH tbl AS MATERIALIZED (
  SELECT c.oid, s.nspname, c.relname FROM pg_class c JOIN pg_namespace s ON s.oid = c.relnamespace
   WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition
     AND s.nspname NOT IN ('pg_catalog', 'information_schema') AND s.nspname NOT LIKE 'pg\_%'),
tree AS (SELECT oid AS top, oid AS rel FROM tbl UNION SELECT tbl.oid, t.relid FROM tbl, pg_partition_tree(tbl.oid) t),
held (rel, k, how) AS (
  SELECT conrelid, k, CASE contype WHEN 'f' THEN 'null' ELSE 'none' END FROM pg_constraint, unnest(conkey) k WHERE contype IN ('f', 'c', 'x')
  UNION ALL SELECT confrelid, k, 'none' FROM pg_constraint, unnest(confkey) k WHERE contype = 'f'
  UNION ALL SELECT i.indrelid, k, CASE WHEN to_jsonb(i) ->> 'indnullsnotdistinct' = 'true' THEN 'nnd' ELSE 'unique' END
    FROM pg_index i, unnest(i.indkey::int2[]) k WHERE i.indisunique AND k > 0
  UNION ALL SELECT i.indrelid, a.attnum, 'none' FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum > 0
    WHERE (i.indisunique OR i.indisexclusion) AND concat(i.indexprs::text, i.indpred::text) ~ (':varattno ' || a.attnum || ' ')
  UNION ALL SELECT p.partrelid, k, 'none' FROM pg_partitioned_table p, unnest(p.partattrs::int2[]) k WHERE k > 0
  UNION ALL SELECT p.partrelid, a.attnum, 'none' FROM pg_partitioned_table p JOIN pg_attribute a ON a.attrelid = p.partrelid AND a.attnum > 0
    WHERE p.partexprs::text ~ (':varattno ' || a.attnum || ' ')),
hows AS (
  SELECT tree.top, ca.attname, array_agg(held.how) AS hows FROM tree JOIN held ON held.rel = tree.rel
    JOIN pg_attribute ca ON ca.attrelid = held.rel AND ca.attnum = held.k GROUP BY 1, 2)
SELECT json_build_object('path', tbl.nspname || '.' || tbl.relname || '.' || a.attname, 'type', format_type(a.atttypid, a.atttypmod),
       'not_null', a.attnotnull OR 'nnd' = ANY (h.hows), 'unique', coalesce(h.hows && ARRAY['unique', 'nnd'], false),
       'takes', CASE WHEN 'none' = ANY (h.hows) OR t.typtype = 'd' AND (t.typnotnull OR EXISTS (SELECT 1 FROM pg_constraint d WHERE d.contypid = t.oid)) THEN 'nothing'
                     WHEN 'null' = ANY (h.hows) OR t.typcategory = 'A' OR t.typtype <> 'b' THEN 'null' ELSE 'any' END)::text
  FROM tbl JOIN pg_attribute a ON a.attrelid = tbl.oid JOIN pg_type t ON t.oid = a.atttypid
  LEFT JOIN hows h ON h.top = tbl.oid AND h.attname = a.attname
 WHERE a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated = '' AND a.attidentity <> 'a'
   AND strpos(tbl.nspname || tbl.relname || a.attname, '.') = 0
 ORDER BY tbl.nspname, tbl.relname, a.attnum";

/// One line per object in the user's schemas, for `roll_previews::schema::diff`: tables, views,
/// sequences, columns (type, nullability, default), indexes, constraints, enums, functions and
/// extensions.
const SCHEMA_SQL: &str = r"
WITH s AS (SELECT oid, nspname FROM pg_namespace WHERE nspname NOT IN ('pg_catalog', 'information_schema') AND nspname NOT LIKE 'pg\_%')
SELECT line FROM (
  SELECT format('%s %I.%I', CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'S' THEN 'sequence' WHEN 'f' THEN 'foreign table' ELSE 'table' END, s.nspname, c.relname) AS line
    FROM pg_class c JOIN s ON s.oid = c.relnamespace WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
  UNION ALL
  SELECT format('%s %I.%I.%I %s%s%s', CASE WHEN c.relkind IN ('v', 'm') THEN 'view column' ELSE 'column' END, s.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod),
                CASE WHEN a.attnotnull THEN ' not null' ELSE '' END, coalesce(' default ' || pg_get_expr(d.adbin, d.adrelid), ''))
    FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN s ON s.oid = c.relnamespace
    LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
  UNION ALL
  SELECT format('index %s', pg_get_indexdef(i.indexrelid))
    FROM pg_index i JOIN pg_class ci ON ci.oid = i.indexrelid JOIN s ON s.oid = ci.relnamespace
  UNION ALL
  SELECT format('constraint %I on %I.%I %s', k.conname, s.nspname, c.relname, pg_get_constraintdef(k.oid))
    FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid JOIN s ON s.oid = c.relnamespace
  UNION ALL
  SELECT format('type %I.%I enum (%s)', s.nspname, t.typname, (SELECT string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder) FROM pg_enum e WHERE e.enumtypid = t.oid))
    FROM pg_type t JOIN s ON s.oid = t.typnamespace WHERE t.typtype = 'e'
  UNION ALL
  SELECT format('function %I.%I(%s) returns %s', s.nspname, p.proname, pg_get_function_identity_arguments(p.oid), coalesce(pg_get_function_result(p.oid), 'nothing'))
    FROM pg_proc p JOIN s ON s.oid = p.pronamespace
  UNION ALL
  SELECT format('extension %I %s', e.extname, e.extversion) FROM pg_extension e
) x ORDER BY line";

// ── Redis ──

pub struct Redis(pub Api);

#[async_trait]
impl Brancher for Redis {
    fn kind(&self) -> Kind {
        Kind::Redis
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let d = match self.0.get(&format!("/v1/redis/databases/{}", seg(name))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let parent_id = self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await.map(|p| p["id"].clone()).unwrap_or(Value::Null);
        Ok((d["parent_id"] == parent_id && !parent_id.is_null()).then(|| s(&d["id"])).flatten())
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        anyhow::ensure!(mode == Mode::Branch, "a Redis branch can't be structure-only");
        match self.0.post(&format!("/v1/redis/databases/{}/branches", seg(parent)), json!({ "name": name })).await {
            Ok(v) => Ok(Branched::plain(s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a database without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                // Names are global: the database named `name` is ours only if it's the parent's.
                let d = self.0.get(&format!("/v1/redis/databases/{}", seg(name))).await?;
                let parent_id = self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await?["id"].clone();
                anyhow::ensure!(d["parent_id"] == parent_id, "a Redis database named {name} exists and isn't a branch of {parent}");
                Ok(Branched::plain(s(&d["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    /// A branch of the preview's own branch: an earlier push's data (redis-control branches any
    /// database, and keeps a parent's history while its branches use it).
    async fn snapshot(&self, _parent: &str, branch: &str, name: &str) -> anyhow::Result<Option<Branched>> {
        self.branch(branch, name, Mode::Branch).await.map(Some)
    }

    /// Its parent is the preview's branch it was copied from (gone after a reset: the guard then
    /// reads the copy's own `parent_id`).
    async fn delete_snapshot(&self, _parent: &str, from: &str, snapshot: &str, _kept: Option<&str>, name: &str) -> anyhow::Result<()> {
        self.delete(from, snapshot, name).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let d = match self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let parent_id = match self.0.get(&format!("/v1/redis/databases/{}", seg(parent))).await {
            Ok(p) => p["id"].clone(),
            // The parent is gone: the branch still must carry our name.
            Err(e) if is_404(&e) => d["parent_id"].clone(),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(d["name"] == name && d["parent_id"] == parent_id && !parent_id.is_null(), "refusing to delete Redis database {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/redis/databases/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let d = self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await?;
        let mut v = vec![("REDIS_URL".into(), s(&d["redis_url"]).ok_or_else(|| anyhow::anyhow!("no redis_url"))?)];
        // Its HTTP API too (Upstash's REST names, as roll apps' link gives them).
        if let Some(u) = s(&d["rest_url"]) {
            v.push(("REDIS_REST_URL".into(), u));
            v.push(("REDIS_REST_TOKEN".into(), s(&d["rest_token"]).unwrap_or_default()));
        }
        Ok(v)
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let u = self.0.get(&format!("/v1/redis/databases/{}/usage?days=30", seg(branch))).await?;
        let d = self.0.get(&format!("/v1/redis/databases/{}", seg(branch))).await?;
        let bytes = u["storage_bytes"].as_i64().unwrap_or(0);
        let mb = d["max_memory_mb"].as_f64().unwrap_or(256.0);
        Ok(Usage {
            bytes,
            storage_usd_month: gb(bytes) * 0.023,
            // Memory is billed while loaded: a busy preview fills it up to its peak.
            active_usd_hour: (bytes as f64 / (1u64 << 30) as f64).max(mb.min(64.0) / 1024.0) * 0.0067,
            month_to_date_usd: u["cost"]["total_usd"].as_f64().unwrap_or(0.0),
        })
    }
}

// ── NoSQL ──

pub struct Nosql(pub Api);

#[async_trait]
impl Brancher for Nosql {
    fn kind(&self) -> Kind {
        Kind::Nosql
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let d = match self.0.get(&format!("/v1/nosql/databases/{}", seg(name))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let parent_id = self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await.map(|p| p["id"].clone()).unwrap_or(Value::Null);
        Ok((d["parent_id"] == parent_id && !parent_id.is_null()).then(|| s(&d["id"])).flatten())
    }

    /// From the parent's history (`at`, unix seconds).
    async fn branch_at(&self, parent: &str, name: &str, mode: Mode, at: i64) -> anyhow::Result<Option<Branched>> {
        let v = self.0.post(&format!("/v1/nosql/databases/{}/branches", seg(parent)), json!({ "name": name, "schema_only": mode == Mode::Schema, "at": at })).await?;
        Ok(Some(Branched::plain(s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a database without an id"))?)))
    }

    /// In place, to the parent as it was then (same id, endpoint and credentials).
    async fn reset_at(&self, _parent: &str, branch: &str, _kept: Option<&str>, _name: &str, _mode: Mode, at: i64) -> anyhow::Result<Option<Branched>> {
        self.0.post(&format!("/v1/nosql/databases/{}/reset", seg(branch)), json!({ "at": at })).await?;
        Ok(Some(Branched::plain(branch)))
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let body = json!({ "name": name, "schema_only": mode == Mode::Schema });
        match self.0.post(&format!("/v1/nosql/databases/{}/branches", seg(parent)), body).await {
            Ok(v) => Ok(Branched::plain(s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a database without an id"))?)),
            Err(e) if conflict_is_ours(&e) => {
                let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(name))).await?;
                let parent_id = self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await?["id"].clone();
                anyhow::ensure!(d["parent_id"] == parent_id, "a NoSQL database named {name} exists and isn't a branch of {parent}");
                Ok(Branched::plain(s(&d["id"]).unwrap_or_default()))
            }
            Err(e) => Err(e),
        }
    }

    /// Merge masks onto the branch's own policy (inherited from its parent at branching).
    async fn mask(&self, _parent: &str, branch: &str, _kept: Option<&str>, columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await?;
        let mut policy = d["masking"].as_object().cloned().unwrap_or_default();
        for (col, f) in columns {
            policy.insert(col.clone(), json!(f));
        }
        self.0.call(Method::PATCH, &format!("/v1/nosql/databases/{}", seg(branch)), Some(json!({ "masking": policy }))).await.map(|_| ())
    }

    async fn mask_problems(&self, parent: &str, columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<Vec<String>> {
        let schema = self.0.get(&format!("/v1/nosql/databases/{}/schema", seg(parent))).await?;
        Ok(roll_previews::mask::nosql_key_masks(&schema, columns))
    }

    /// The parent's columns that can be masked (not a key's: `mask::nosql_columns`).
    async fn columns(&self, parent: &str, max: usize) -> anyhow::Result<Vec<roll_previews::mask::Column>> {
        let schema = self.0.get(&format!("/v1/nosql/databases/{}/schema", seg(parent))).await?;
        Ok(roll_previews::mask::nosql_columns(&schema, max))
    }

    /// NoSQL resets in place: same id, endpoint and credentials.
    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        match self.0.post(&format!("/v1/nosql/databases/{}/reset", seg(branch)), json!({})).await {
            Ok(_) => Ok(Branched::plain(branch)),
            Err(e) if is_404(&e) => self.branch(parent, name, mode).await,
            Err(e) => Err(e),
        }
    }

    /// A branch of the preview's own database: an earlier push's data. A NoSQL branch inherits
    /// its parent's masking policy, so the copy reads masked as the preview's branch does (and
    /// goes when the rules change).
    async fn snapshot(&self, _parent: &str, branch: &str, name: &str) -> anyhow::Result<Option<Branched>> {
        self.branch(branch, name, Mode::Branch).await.map(Some)
    }

    async fn delete_snapshot(&self, _parent: &str, from: &str, snapshot: &str, _kept: Option<&str>, name: &str) -> anyhow::Result<()> {
        self.delete(from, snapshot, name).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let d = match self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await {
            Ok(d) => d,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let parent_id = match self.0.get(&format!("/v1/nosql/databases/{}", seg(parent))).await {
            Ok(p) => p["id"].clone(),
            Err(e) if is_404(&e) => d["parent_id"].clone(),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(d["name"] == name && d["parent_id"] == parent_id && !parent_id.is_null(), "refusing to delete NoSQL database {branch}: it isn't {parent}'s branch {name}");
        self.0.delete(&format!("/v1/nosql/databases/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, _kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let d = self.0.get(&format!("/v1/nosql/databases/{}", seg(branch))).await?;
        let mut v = vec![
            ("CASSANDRA_CONTACT_POINTS".into(), s(&d["endpoints"]["cql"]).unwrap_or_default()),
            ("CASSANDRA_USERNAME".into(), s(&d["credentials"]["username"]).unwrap_or_default()),
            ("CASSANDRA_PASSWORD".into(), s(&d["credentials"]["password"]).unwrap_or_default()),
        ];
        if let Some(dc) = s(&d["datacenter"]) {
            v.push(("CASSANDRA_LOCAL_DATACENTER".into(), dc));
        }
        Ok(v)
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let u = self.0.get(&format!("/v1/nosql/databases/{}/usage?days=30", seg(branch))).await?;
        let bytes = u["storage_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: u["cost"]["total_usd"].as_f64().unwrap_or(0.0) })
    }
}

// ── Storage buckets: a copy-on-write branch plus a write key scoped to it ──

pub struct Bucket(pub Api);

impl Bucket {
    /// A fresh write key for the branch only (the secret is shown once, so it's kept).
    async fn key(&self, bucket: &str) -> anyhow::Result<String> {
        let k = self.0.post("/v1/storage/keys", json!({ "name": bucket, "permission": "write", "buckets": [bucket] })).await?;
        let info = self.0.get("/v1/storage").await.unwrap_or(Value::Null);
        Ok(json!({
            "S3_BUCKET": bucket,
            "AWS_ENDPOINT_URL_S3": info["endpoint"],
            "AWS_REGION": info["region"],
            "AWS_ACCESS_KEY_ID": k["id"],
            "AWS_SECRET_ACCESS_KEY": k["secret_access_key"],
        })
        .to_string())
    }

    /// Every key minted for the branch.
    async fn drop_keys(&self, bucket: &str) -> anyhow::Result<()> {
        let keys = self.0.get("/v1/storage/keys").await?;
        for k in keys.as_array().into_iter().flatten() {
            if k["name"] == bucket && k["buckets"].as_array().is_some_and(|b| b.len() == 1 && b[0] == bucket) {
                self.0.delete(&format!("/v1/storage/keys/{}", seg(k["id"].as_str().unwrap_or_default()))).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Brancher for Bucket {
    fn kind(&self) -> Kind {
        Kind::Bucket
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let child = bucket_name(parent, name);
        match self.0.get(&format!("/v1/storage/buckets/{}", seg(&child))).await {
            Ok(b) if b["parent"]["name"] == parent => Ok(Some(child)),
            Ok(_) => Ok(None),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        anyhow::ensure!(mode == Mode::Branch, "a bucket branch can't be structure-only");
        // Bucket names are global: the parent's name keeps it readable, the preview's makes it
        // unique (63 characters at most).
        let child = bucket_name(parent, name);
        match self.0.post(&format!("/v1/storage/buckets/{}/branches", seg(parent)), json!({ "name": child })).await {
            Ok(_) => {}
            Err(e) if conflict_is_ours(&e) => {
                let b = self.0.get(&format!("/v1/storage/buckets/{}", seg(&child))).await?;
                anyhow::ensure!(b["parent"]["name"] == parent, "a bucket named {child} exists and isn't a branch of {parent}");
            }
            Err(e) => return Err(e),
        }
        // A key from before a crash can't be read back: replace it.
        self.drop_keys(&child).await?;
        Ok(Branched { r#ref: child.clone(), kept: Some(self.key(&child).await?) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    /// A branch of the preview's branch bucket (reads resolve through every ancestor), with a
    /// key of its own: an earlier push's files as its head left them. roll storage refuses to
    /// delete a bucket with branches, so copies always go first (eviction, reset, teardown).
    async fn snapshot(&self, _parent: &str, branch: &str, name: &str) -> anyhow::Result<Option<Branched>> {
        self.branch(branch, name, Mode::Branch).await.map(Some)
    }

    async fn delete_snapshot(&self, _parent: &str, from: &str, snapshot: &str, _kept: Option<&str>, name: &str) -> anyhow::Result<()> {
        self.delete(from, snapshot, name).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == bucket_name(parent, name) && branch != parent, "refusing to delete bucket {branch}: it isn't {parent}'s branch {name}");
        let b = match self.0.get(&format!("/v1/storage/buckets/{}", seg(branch))).await {
            Ok(b) => b,
            Err(e) if is_404(&e) => return self.drop_keys(branch).await,
            Err(e) => return Err(e),
        };
        anyhow::ensure!(b["parent"]["name"] == parent, "refusing to delete bucket {branch}: it isn't a branch of {parent}");
        // `force` empties the branch: only its own rows (the parent's objects are the parent's).
        self.0.delete(&format!("/v1/storage/buckets/{}?force=true", seg(branch))).await?;
        self.drop_keys(branch).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the bucket branch {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let b = self.0.get(&format!("/v1/storage/buckets/{}", seg(branch))).await?;
        let bytes = b["bytes"].as_i64().unwrap_or(0) + b["trash_bytes"].as_i64().unwrap_or(0) + b["version_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

/// `<parent>-<preview branch name>`, cut to S3's 63 characters (the preview part kept whole).
pub fn bucket_name(parent: &str, name: &str) -> String {
    let room = 63usize.saturating_sub(name.len() + 1);
    let head: String = parent.chars().take(room).collect();
    format!("{}-{name}", head.trim_end_matches(['-', '.']))
}

// ── Streams: a copy-on-write branch (`branch`), or a stream with the parent's topics (`schema`) ──

pub struct Stream(pub Api);

#[async_trait]
impl Brancher for Stream {
    fn kind(&self) -> Kind {
        Kind::Stream
    }

    async fn find(&self, _parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        match self.0.get(&format!("/v1/streams/{}", seg(name))).await {
            Ok(v) if v["name"] == name => Ok(s(&v["id"])),
            Ok(_) => Ok(None),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let p = self.0.get(&format!("/v1/streams/{}", seg(parent))).await?;
        // `branch`: the parent's records (shared chunks) and committed offsets, on the parent's
        // owner. `schema`: a new stream, then the parent's topics.
        let made = match mode {
            Mode::Branch => self.0.post(&format!("/v1/streams/{}/branches", seg(parent)), json!({ "name": name })).await,
            Mode::Schema => self.0.post("/v1/streams", json!({ "name": name, "settings": p["settings"] })).await,
        };
        let made = match made {
            Ok(v) => v,
            Err(e) if conflict_is_ours(&e) => {
                // From before a crash: a fresh key, since the first secret is gone.
                let v = self.0.get(&format!("/v1/streams/{}", seg(name))).await?;
                let key = self.0.post(&format!("/v1/streams/{}/keys", seg(name)), json!({ "label": "roll preview" })).await?;
                json!({ "id": v["id"], "bootstrap": v["bootstrap"], "connect": { "bootstrap": v["bootstrap"], "username": key["username"], "secret": key["secret"] } })
            }
            Err(e) => return Err(e),
        };
        let id = s(&made["id"]).ok_or_else(|| anyhow::anyhow!("a stream without an id"))?;
        // A branch has its topics already (and a stream from before a crash is either whole or
        // gets the missing ones here).
        for t in self.0.get(&format!("/v1/streams/{}/topics", seg(parent))).await?.as_array().into_iter().flatten() {
            let body = json!({ "name": t["name"], "partitions": t["partitions"], "configs": t["configs"] });
            match self.0.post(&format!("/v1/streams/{}/topics", seg(&id)), body).await {
                Ok(_) => {}
                Err(e) if conflict_is_ours(&e) => {}
                Err(e) => return Err(e),
            }
        }
        let c = &made["connect"];
        let kept = json!({
            "KAFKA_BOOTSTRAP_SERVERS": c["bootstrap"],
            "KAFKA_SASL_MECHANISM": "SCRAM-SHA-256",
            "KAFKA_SASL_USERNAME": c["username"],
            "KAFKA_SASL_PASSWORD": c["secret"],
        });
        Ok(Branched { r#ref: id, kept: Some(kept.to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    /// A branch of the preview's own stream (any stream branches: a new stream whose chunk rows
    /// name the parent's objects, independent after), with a key of its own.
    async fn snapshot(&self, _parent: &str, branch: &str, name: &str) -> anyhow::Result<Option<Branched>> {
        self.branch(branch, name, Mode::Branch).await.map(Some)
    }

    /// Its `branch_of` is the preview's stream it was copied from (unchecked once that's gone).
    async fn delete_snapshot(&self, _parent: &str, from: &str, snapshot: &str, _kept: Option<&str>, name: &str) -> anyhow::Result<()> {
        self.delete(from, snapshot, name).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let v = match self.0.get(&format!("/v1/streams/{}", seg(branch))).await {
            Ok(v) => v,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        anyhow::ensure!(v["name"] == name && branch != parent, "refusing to delete stream {branch}: it isn't the preview's stream {name}");
        if let Some(of) = s(&v["branch_of"]["stream"]) {
            match self.0.get(&format!("/v1/streams/{}", seg(parent))).await {
                Ok(p) => anyhow::ensure!(p["id"] == of.as_str(), "refusing to delete stream {branch}: it's a branch of {of}, not of {parent}"),
                Err(e) if is_404(&e) => {}
                Err(e) => return Err(e),
            }
        }
        self.0.delete(&format!("/v1/streams/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the stream {branch} has no key: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        // Object storage counts only the branch's own bytes: what it shares with its parent is
        // the parent's.
        let u = self.0.get(&format!("/v1/streams/{}/usage?hours=24", seg(branch))).await?;
        let last = u["storage"].as_array().and_then(|a| a.last()).cloned().unwrap_or(Value::Null);
        let bytes = last["local_bytes"].as_i64().unwrap_or(0) + u["remote"]["own_bytes"].as_i64().unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

// ── Queues: a vhost with the parent's definitions, no messages ──

pub struct Queue(pub Api);

#[async_trait]
impl Brancher for Queue {
    fn kind(&self) -> Kind {
        Kind::Queue
    }

    async fn find(&self, _parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        Ok(vhosts.as_array().is_some_and(|a| a.iter().any(|v| v["name"] == name)).then(|| name.to_string()))
    }

    async fn branch(&self, parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        let parent_v = vhosts.as_array().and_then(|a| a.iter().find(|v| v["name"] == parent).cloned()).ok_or_else(|| anyhow::anyhow!("no vhost {parent}"))?;
        let exists = vhosts.as_array().is_some_and(|a| a.iter().any(|v| v["name"] == name));
        if !exists {
            let body = json!({ "name": name, "description": format!("roll preview of {parent}"), "default_queue_type": parent_v["default_queue_type"] });
            match self.0.post("/v1/queues/vhosts", body).await {
                Ok(_) => {}
                Err(e) if conflict_is_ours(&e) => {}
                Err(e) => return Err(e),
            }
        }
        // The parent's exchanges, queues, bindings, policies and parameters, moved to the branch.
        let defs = self.0.get("/v1/queues/definitions").await?;
        let mut copy = json!({});
        for key in ["exchanges", "queues", "bindings", "policies", "parameters"] {
            let items: Vec<Value> = defs[key]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| i["vhost"] == parent)
                // Shovels and federation move messages in from elsewhere (production, with its
                // credentials): a branch never gets them.
                .filter(|i| !(key == "parameters" && i["component"].as_str().is_some_and(|c| c.starts_with("shovel") || c.starts_with("federation"))))
                .map(|i| {
                    let mut i = i.clone();
                    i["vhost"] = json!(name);
                    if key == "policies"
                        && let Some(def) = i["definition"].as_object_mut()
                    {
                        def.retain(|k, _| !k.starts_with("federation-upstream"));
                    }
                    i
                })
                .collect();
            copy[key] = json!(items);
        }
        self.0.post("/v1/queues/definitions", copy).await?;
        let c = self.0.post(&format!("/v1/queues/vhosts/{}/credentials", seg(name)), json!({})).await?;
        Ok(Branched { r#ref: name.to_string(), kept: Some(json!({ "AMQP_URL": c["urls"]["amqp"] }).to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == name && branch != parent, "refusing to delete vhost {branch}: it isn't the preview's vhost {name}");
        self.0.delete(&format!("/v1/queues/vhosts/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the vhost {branch} has no credentials: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let vhosts = self.0.get("/v1/queues/vhosts").await?;
        let bytes = vhosts.as_array().and_then(|a| a.iter().find(|v| v["name"] == branch)).and_then(|v| v["stored_bytes"].as_i64()).unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

// ── Tasks: an empty tenant (copied schedules would call production) ──

pub struct Tasks {
    pub api: Api,
    /// Where services reach the tasks API.
    pub public: String,
}

#[async_trait]
impl Brancher for Tasks {
    fn kind(&self) -> Kind {
        Kind::Tasks
    }

    async fn find(&self, _parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        let t = self.api.get("/_roll/tenants").await?;
        Ok(t["tenants"].as_array().is_some_and(|a| a.iter().any(|x| x == name)).then(|| name.to_string()))
    }

    async fn branch(&self, _parent: &str, name: &str, _mode: Mode) -> anyhow::Result<Branched> {
        let token = match self.api.post("/_roll/tenants", json!({ "id": name })).await {
            Ok(v) => v["token"].clone(),
            Err(e) if conflict_is_ours(&e) => self.api.post(&format!("/_roll/tenants/{}/keys", seg(name)), json!({ "name": "roll preview" })).await?["token"].clone(),
            Err(e) => return Err(e),
        };
        let kept = json!({ "ROLL_TASKS_URL": format!("{}/t/{name}", self.public.trim_end_matches('/')), "ROLL_TASKS_TOKEN": token });
        Ok(Branched { r#ref: name.to_string(), kept: Some(kept.to_string()) })
    }

    async fn reset(&self, parent: &str, branch: &str, _kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        anyhow::ensure!(branch == name && branch != parent && branch != "default", "refusing to delete tasks tenant {branch}: it isn't the preview's tenant {name}");
        self.api.delete(&format!("/_roll/tenants/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the tenant {branch} has no key: reset it"))?;
        let mut vars: Vec<(String, String)> = kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect();
        // Its own webhook signing keys (roll apps' `tasks` link names them): the preview verifies
        // its tenant's deliveries with these, never production's. Read at each start, so a
        // rotation reaches the next one.
        // The tenant's own route, as the tenant (its token, the one its services hold).
        let token = s(&kept["ROLL_TASKS_TOKEN"]).ok_or_else(|| anyhow::anyhow!("the tenant {branch} has no token: reset it"))?;
        let keys = self.api.get_as(&format!("/t/{}/v1/tasks/keys", seg(branch)), &token).await.map_err(|e| anyhow::anyhow!("reading the tenant {branch}'s signing keys: {e:#}"))?;
        for (name, field) in [("ROLL_TASKS_SIGNING_KEY", "current"), ("ROLL_TASKS_NEXT_SIGNING_KEY", "next")] {
            vars.push((name.to_string(), s(&keys[field]).ok_or_else(|| anyhow::anyhow!("the tenant {branch} has no {field} signing key"))?));
        }
        Ok(vars)
    }

    async fn usage(&self, _parent: &str, _branch: &str) -> anyhow::Result<Usage> {
        Ok(Usage::default())
    }
}

// ── Search: a zero-copy branch of an Elasticsearch-compatible cluster (roll search) ──

/// Masks one field (`params.path`) of a document: `redact`, `remove` or `email`. It goes through
/// arrays at every step (an object field, `nested` ones included) and literal dotted keys
/// (`{"user.email": …}`) as well as objects, and a document it changes nothing in is a `noop`.
const SEARCH_MASK_SCRIPT: &str = "boolean mask(def m, List p, int i, String rule, String id) { \
if (m instanceof List) { boolean c = false; for (def e : m) { if (mask(e, p, i, rule, id)) { c = true; } } return c; } \
if (!(m instanceof Map)) { return false; } \
boolean c = false; String key = ''; \
for (int j = i; j < p.size(); j++) { \
key = j == i ? p.get(j) : key + '.' + p.get(j); \
if (!m.containsKey(key)) { continue; } \
if (j == p.size() - 1) { \
if (rule == 'remove') { m.remove(key); } else if (rule == 'email') { m.put(key, 'user-' + id + '@example.invalid'); } else { m.put(key, '[masked]'); } \
c = true; \
} else if (mask(m.get(key), p, j + 1, rule, id)) { c = true; } \
} \
return c; } \
if (!mask(ctx._source, params.path, 0, params.rule, ctx._id)) { ctx.op = 'noop'; }";

/// A `_mapping` answer as lines: each index (not the system ones, `.…`), and each field with its
/// type, nested ones by path (`field users.address.city text`) and multi-fields too. Bounded.
/// One Postgres endpoint's schema lines (`SCHEMA_SQL`), as `role` when given (a branch's own),
/// with only the catalog's names in reach.
/// The tables a branch's role can read, with about how many rows each holds, at most 200.
const BROWSE_TABLES_SQL: &str = "SELECT n.nspname || '.' || c.relname, c.reltuples::bigint::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE c.relkind IN ('r', 'p', 'v', 'm') AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_toast%' \
     AND has_table_privilege(c.oid, 'SELECT') ORDER BY 1 LIMIT 200";

/// A table's first rows (one more than are shown: whether there are more), its name quoted
/// (`schema.table`, split at the first dot).
fn browse_rows_sql(table: &str) -> Option<String> {
    let (schema, name) = table.split_once('.')?;
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    (!schema.is_empty() && !name.is_empty()).then(|| format!("SELECT * FROM {}.{} LIMIT {}", q(schema), q(name), roll_previews::BROWSE_ROWS + 1))
}

/// `sql` on endpoint `ep` as `role`, in a read-only transaction of at most 5 s, with only the
/// catalog in its path: its result (the one before the commit's).
async fn pg_read_as(api: &Api, ep: &str, role: &str, sql: &str) -> anyhow::Result<Value> {
    let q = format!(
        "BEGIN READ ONLY; SET LOCAL ROLE \"{}\"; SET LOCAL search_path = pg_catalog, pg_temp; SET LOCAL statement_timeout = '5s'; {sql}; COMMIT",
        role.replace('"', "\"\"")
    );
    let v = api.post(&format!("/v1/endpoints/{}/query", seg(ep)), json!({ "sql": q })).await?;
    v["results"].as_array().and_then(|r| r.iter().rev().find(|x| x["columns"].as_array().is_some_and(|c| !c.is_empty())).cloned()).ok_or_else(|| anyhow::anyhow!("the branch answered nothing"))
}

async fn pg_schema_lines(api: &Api, ep: Option<String>, role: Option<String>) -> anyhow::Result<Vec<String>> {
    let ep = ep.ok_or_else(|| anyhow::anyhow!("a Postgres branch without an endpoint"))?;
    let set_role = role.map(|r| format!("SET ROLE \"{}\"; ", r.replace('"', "\"\""))).unwrap_or_default();
    let sql = format!("{set_role}SET search_path = pg_catalog, pg_temp; {SCHEMA_SQL}");
    let v = api.post(&format!("/v1/endpoints/{}/query", seg(&ep)), json!({ "sql": sql })).await?;
    let rows = v["results"].as_array().and_then(|r| r.last()).and_then(|r| r["rows"].as_array()).cloned().unwrap_or_default();
    Ok(rows.iter().filter_map(|r| r[0].as_str().map(str::to_string)).collect())
}

fn mapping_lines(v: &Value, skip: &std::collections::BTreeSet<String>) -> Vec<String> {
    fn walk(props: &Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
        let Some(props) = props.as_object() else { return };
        for (name, f) in props {
            if out.len() >= 5000 || depth > 20 {
                return;
            }
            let path = format!("{prefix}.{name}");
            let ty = f["type"].as_str().unwrap_or(if f.get("properties").is_some() { "object" } else { "?" });
            out.push(format!("field {path} {ty}"));
            walk(&f["properties"], &path, depth + 1, out);
            walk(&f["fields"], &path, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for (index, m) in v.as_object().into_iter().flatten().filter(|(i, _)| !i.starts_with('.') && !skip.contains(*i)) {
        out.push(format!("index {index}"));
        walk(&m["mappings"]["properties"], index, 0, &mut out);
    }
    out
}

/// A search cluster's indices, hidden ones too (a data stream's backing indices, `.ds-…`, which
/// masks can name).
const CAT_INDICES: &str = "/_cat/indices?h=index&format=json&expand_wildcards=all";

pub struct Search(pub Api);

impl Search {
    /// What the preview's services get for the branch: a data-only credential of it, as
    /// production's link to the cluster gets (roll search's keys route, `access: data`: indices,
    /// templates, pipelines, lifecycle; no keys, users or `_security`), never the branch's own
    /// login (a pull request's code runs here). `ELASTICSEARCH_URL` without a login and
    /// `ELASTICSEARCH_API_KEY`, or for an OpenSearch cluster its data-only user in the URL (so
    /// `OPENSEARCH_USERNAME` and the rest are that user's). No data credential, no link: the
    /// branch fails (and is made again at the next converge), it never falls back to the login.
    async fn kept_with_key(&self, v: &Value, name: &str) -> anyhow::Result<String> {
        let url = s(&v["url"]).ok_or_else(|| anyhow::anyhow!("a search branch without a url"))?;
        let id = s(&v["id"]).unwrap_or_default();
        let k = self
            .0
            .post(&format!("/v1/search/clusters/{}/keys", seg(&id)), json!({ "name": format!("roll preview {name}"), "access": "data" }))
            .await
            .map_err(|e| anyhow::anyhow!("a data-only credential for the search branch couldn't be made ({e:#}): it doesn't get the branch's own login instead"))?;
        if k["access"].as_str() != Some("data") {
            // Never kept: revoked at once, as roll secrets does with one.
            if let Some(key) = s(&k["id"]) {
                let _ = self.0.delete(&format!("/v1/search/clusters/{}/keys/{}", seg(&id), seg(&key))).await;
            }
            anyhow::bail!("roll search made a {} key for the search branch, not a data one: revoked, and the branch gets no link", k["access"]);
        }
        if let Some(key) = s(&k["api_key"]) {
            return Ok(json!({ "ELASTICSEARCH_URL": url, "ELASTICSEARCH_API_KEY": key }).to_string());
        }
        if let (Some(u), Some(p)) = (s(&k["username"]), s(&k["password"])) {
            let with = match url.split_once("://") {
                Some((scheme, rest)) => format!("{scheme}://{}:{}@{rest}", seg(&u), seg(&p)),
                None => anyhow::bail!("a search branch's url without a scheme"),
            };
            return Ok(json!({ "ELASTICSEARCH_URL": with, "OPENSEARCH_USERNAME": u, "OPENSEARCH_PASSWORD": p }).to_string());
        }
        anyhow::bail!("roll search's data credential for the search branch holds no key or user")
    }
}

#[async_trait]
impl Brancher for Search {
    fn kind(&self) -> Kind {
        Kind::Search
    }

    /// A string mask (`redact`, `email`) on a field of the parent's that isn't text (a vector, a
    /// number) would fail the masking, and the preview: named when the rule is set.
    async fn mask_problems(&self, parent: &str, fields: &std::collections::BTreeMap<String, String>) -> anyhow::Result<Vec<String>> {
        // Each index the rules name, read as itself (an alias or pattern answers with its real
        // indices, and only those are checked); one the parent doesn't have yet is none.
        let listed = self.0.post(&format!("/v1/search/clusters/{}/request", seg(parent)), json!({ "method": "GET", "path": CAT_INDICES })).await?;
        let indices: Vec<String> = serde_json::from_str::<Vec<Value>>(listed["body"].as_str().unwrap_or("[]")).unwrap_or_default().iter().filter_map(|x| x["index"].as_str().map(str::to_string)).collect();
        let mut by_index: std::collections::BTreeMap<&str, std::collections::BTreeMap<String, String>> = Default::default();
        for (spec, rule) in fields {
            if let Some((index, _)) = roll_previews::mask::split_mask(spec, &indices) {
                by_index.entry(index).or_default().insert(spec.clone(), rule.clone());
            }
        }
        let mut out = Vec::new();
        for (index, specs) in by_index {
            let v = self.0.post(&format!("/v1/search/clusters/{}/request", seg(parent)), json!({ "method": "GET", "path": format!("/{}/_mapping", seg(index)) })).await?;
            match v["status"].as_u64() {
                Some(200) => {}
                Some(404) => continue,
                s => anyhow::bail!("reading {parent}'s {index} mapping: {s:?}"),
            }
            let mapping: Value = serde_json::from_str(v["body"].as_str().unwrap_or("{}"))?;
            out.extend(roll_previews::mask::search_field_masks(&mapping, &specs, &indices));
        }
        Ok(out)
    }

    /// The parent's fields from its indices' mappings, through the operator's console request
    /// (as `schema`), one index at a time until `max` (so a large cluster's mappings aren't
    /// read whole): its own indices and its data streams' backing ones, not system indices.
    async fn columns(&self, parent: &str, max: usize) -> anyhow::Result<Vec<roll_previews::mask::Column>> {
        let console = format!("/v1/search/clusters/{}/request", seg(parent));
        let listed = self.0.post(&console, json!({ "method": "GET", "path": CAT_INDICES })).await?;
        anyhow::ensure!(listed["status"].as_u64() == Some(200), "listing {parent}'s indices: {}", listed["status"]);
        let mut indices: Vec<String> = serde_json::from_str::<Vec<Value>>(listed["body"].as_str().unwrap_or("[]"))?.iter().filter_map(|x| s(&x["index"])).collect();
        indices.retain(|i| roll_previews::mask::search_index_offered(i));
        indices.sort();
        let mut out = Vec::new();
        for index in indices {
            if out.len() >= max {
                break;
            }
            let v = self.0.post(&console, json!({ "method": "GET", "path": format!("/{}/_mapping", seg(&index)) })).await?;
            match v["status"].as_u64() {
                Some(200) => {}
                // Deleted since it was listed.
                Some(404) => continue,
                s => anyhow::bail!("reading {parent}'s {index} mapping: {s:?}"),
            }
            let body: Value = serde_json::from_str(v["body"].as_str().unwrap_or("{}"))?;
            out.extend(roll_previews::mask::search_columns(&body, max - out.len()));
        }
        Ok(out)
    }

    async fn find(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        match self.0.get(&format!("/v1/search/clusters/{}", seg(name))).await {
            Ok(c) if c["name"] == name && (c["parent_id"] == parent || c["parent_name"] == parent) => Ok(s(&c["id"])),
            Ok(_) => Ok(None),
            Err(e) if is_404(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        let path = format!("/v1/search/clusters/{}/branches", seg(parent));
        match self.0.post(&path, json!({ "name": name })).await {
            Ok(v) => Ok(Branched { r#ref: s(&v["id"]).ok_or_else(|| anyhow::anyhow!("a search branch without an id"))?, kept: Some(self.kept_with_key(&v, name).await?) }),
            // From before a crash: its password is gone, and it holds nothing yet. Make it again.
            Err(e) if conflict_is_ours(&e) => {
                let c = self.0.get(&format!("/v1/search/clusters/{}", seg(name))).await?;
                self.delete(parent, &s(&c["id"]).unwrap_or_default(), name).await?;
                let v = self.0.post(&path, json!({ "name": name })).await?;
                let _ = mode;
                Ok(Branched { r#ref: s(&v["id"]).unwrap_or_default(), kept: Some(self.kept_with_key(&v, name).await?) })
            }
            Err(e) => Err(e),
        }
    }

    async fn reset(&self, parent: &str, branch: &str, kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched> {
        self.retire(parent, branch, kept).await?;
        self.delete(parent, branch, name).await?;
        self.branch(parent, name, mode).await
    }

    /// A branch of the preview's own branch cluster (roll search branches any cluster; its
    /// manifests share the branch's segments, the masked ones rewritten and expunged, never the
    /// parent's originals), with a data key of its own. Never with `at`: a point-in-time branch
    /// replays the log, which could rebuild what masking removed.
    async fn snapshot(&self, _parent: &str, branch: &str, name: &str) -> anyhow::Result<Option<Branched>> {
        self.branch(branch, name, Mode::Branch).await.map(Some)
    }

    /// As the preview's branch goes: its data key revoked, its password changed, then deleted;
    /// its parent is the branch it was copied from (`parent_id` stays when that's gone).
    async fn delete_snapshot(&self, _parent: &str, from: &str, snapshot: &str, kept: Option<&str>, name: &str) -> anyhow::Result<()> {
        self.retire(from, snapshot, kept).await?;
        self.delete(from, snapshot, name).await
    }

    /// roll search keeps a deleted cluster restorable with its keys and password: so first the
    /// branch's data credential previews handed out is revoked (its id is in what's kept: an API
    /// key is `base64(<id>@<cluster>:<key>)`, an OpenSearch data user `<id>@<cluster>`).
    async fn retire(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<()> {
        let Some(v) = kept.and_then(|k| serde_json::from_str::<Value>(k).ok()) else { return Ok(()) };
        let from_key = s(&v["ELASTICSEARCH_API_KEY"]).and_then(|k| {
            use base64::Engine as _;
            let raw = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(k).ok()?).ok()?;
            raw.split_once(':').and_then(|(id, _)| id.rsplit_once('@')).map(|(id, _)| id.to_string())
        });
        let from_user = s(&v["OPENSEARCH_USERNAME"]).and_then(|u| u.rsplit_once('@').map(|(id, _)| id.to_string()));
        match from_key.or(from_user) {
            Some(key) => self.0.delete(&format!("/v1/search/clusters/{}/keys/{}", seg(branch), seg(&key))).await,
            None => Ok(()),
        }
    }

    /// Its password is changed first: a restored copy then holds no login previews ever handed
    /// out (links made before 2026-11-21 carried it). No change, no delete (tried again).
    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()> {
        let c = match self.0.get(&format!("/v1/search/clusters/{}", seg(branch))).await {
            Ok(c) => c,
            Err(e) if is_404(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        let of_parent = c["parent_id"] == parent || c["parent_name"] == parent;
        anyhow::ensure!(c["name"] == name && of_parent && branch != parent, "refusing to delete search cluster {branch}: it isn't {parent}'s branch {name}");
        self.0.post(&format!("/v1/search/clusters/{}/password", seg(branch)), json!({})).await.map_err(|e| anyhow::anyhow!("changing the search branch's password before it's deleted: {e:#}"))?;
        self.0.delete(&format!("/v1/search/clusters/{}", seg(branch))).await
    }

    async fn connect(&self, _parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
        let kept: Value = kept.and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| anyhow::anyhow!("the search branch {branch} has no credentials: reset it"))?;
        Ok(kept.as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
    }

    /// Both clusters' mappings as lines (`index products`, `field products.price float`), through
    /// the operator's console request (no cluster password needed). An index the parent made after
    /// the branch started (`parent_at`: a connector's first sync, a rollover, a day's `logs-…`)
    /// that the branch doesn't have is left out: production's new index, not one the change drops.
    async fn schema(&self, parent: &str, branch: &str) -> anyhow::Result<Option<(Vec<String>, Vec<String>)>> {
        let get = |id: String, path: &'static str| async move {
            let v = self.0.post(&format!("/v1/search/clusters/{}/request", seg(&id)), json!({ "method": "GET", "path": path })).await?;
            anyhow::ensure!(v["status"].as_u64() == Some(200), "reading {id}'s {path}: {}", v["status"]);
            anyhow::Ok(serde_json::from_str::<Value>(v["body"].as_str().unwrap_or("{}"))?)
        };
        let theirs = get(branch.to_string(), "/_mapping").await?;
        let mut newer = std::collections::BTreeSet::new();
        if let Some(since) = self.0.get(&format!("/v1/search/clusters/{}", seg(branch))).await?["parent_at"].as_i64() {
            let listed = get(parent.to_string(), "/_cat/indices?h=index,creation.date&format=json&expand_wildcards=all").await?;
            for x in listed.as_array().into_iter().flatten() {
                // `_cat` answers numbers as strings; the creation date is in milliseconds.
                let made = x["creation.date"].as_i64().or_else(|| x["creation.date"].as_str().and_then(|s| s.parse().ok()));
                if let (Some(i), Some(made)) = (x["index"].as_str(), made)
                    && made > since * 1000
                    && theirs.get(i).is_none()
                {
                    newer.insert(i.to_string());
                }
            }
        }
        let parents = get(parent.to_string(), "/_mapping").await?;
        Ok(Some((mapping_lines(&parents, &newer), mapping_lines(&theirs, &Default::default()))))
    }

    /// The branch's mapping lines alone, as `schema` reads them.
    async fn branch_schema(&self, _parent: &str, branch: &str) -> anyhow::Result<Option<Vec<String>>> {
        let v = self.0.post(&format!("/v1/search/clusters/{}/request", seg(branch)), json!({ "method": "GET", "path": "/_mapping" })).await?;
        anyhow::ensure!(v["status"].as_u64() == Some(200), "reading {branch}'s mappings: {}", v["status"]);
        Ok(Some(mapping_lines(&serde_json::from_str(v["body"].as_str().unwrap_or("{}"))?, &Default::default())))
    }

    /// Rewrite the masked fields in the branch's documents (`_update_by_query`, Painless): the
    /// parent's segments are shared, so the branch's rewritten documents are its own and the
    /// parent keeps its values. Run again it's harmless: masked values stay masked. Through the
    /// operator's console request, as `schema` reads: roll's own call, so no cluster password and
    /// no IP allowlist (a branch keeps its parent's, which needn't admit this node).
    async fn mask(&self, _parent: &str, branch: &str, _kept: Option<&str>, fields: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        let console = format!("/v1/search/clusters/{}/request", seg(branch));
        // `(status, body)` of one REST request on the branch.
        let run = |method: &'static str, path: String, body: Option<Value>| {
            let console = console.clone();
            async move {
                let mut req = json!({ "method": method, "path": path });
                if let Some(b) = body {
                    req["body"] = b;
                }
                let v = self.0.post(&console, req).await?;
                let text = v["body"].as_str().unwrap_or("").to_string();
                anyhow::Ok((v["status"].as_u64().unwrap_or(0), text))
            }
        };
        let mut masked: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        // The branch's indices, so an index whose name has dots is split off whole.
        let (_, listed) = run("GET", CAT_INDICES.into(), None).await?;
        let indices: Vec<String> = serde_json::from_str::<Vec<Value>>(&listed).unwrap_or_default().iter().filter_map(|x| x["index"].as_str().map(str::to_string)).collect();
        for (spec, rule) in fields {
            let Some((index, path)) = roll_previews::mask::split_mask(spec, &indices).filter(|(i, _)| !i.is_empty()) else { anyhow::bail!("{spec:?} isn't index.field") };
            // Every document, not those `exists` finds: a field that isn't indexed (or has no doc
            // values) is in `_source` all the same.
            let body = json!({
                "query": { "match_all": {} },
                "script": { "lang": "painless", "source": SEARCH_MASK_SCRIPT, "params": { "path": path.split('.').collect::<Vec<_>>(), "rule": rule } },
            });
            let (status, text) = run("POST", format!("/{}/_update_by_query?conflicts=proceed&refresh=true", seg(index)), Some(body)).await?;
            // An index the branch doesn't have holds nothing to mask.
            if status == 404 {
                continue;
            }
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            // A document skipped on a version conflict kept its value: masking isn't done.
            let failures = v["failures"].as_array().map_or(0, Vec::len);
            let conflicts = v["version_conflicts"].as_u64().unwrap_or(0);
            anyhow::ensure!((200..300).contains(&status) && failures == 0 && conflicts == 0, "masking {spec}: {status} {}", text.chars().take(300).collect::<String>());
            masked.insert(index);
        }
        // The documents as they were stay in the segments the branch shares with its parent,
        // marked deleted only in the branch, and a term dictionary keeps their terms
        // (`_terms_enum`, suggesters): every segment that holds one is rewritten as the branch's
        // own without it (as Postgres masking's VACUUM FULL), the parent's untouched.
        for index in masked {
            let (status, _) = run("PUT", format!("/{}/_settings", seg(index)), Some(json!({ "index.merge.policy.expunge_deletes_allowed": 0.0 }))).await?;
            anyhow::ensure!((200..300).contains(&status), "masking {index}: its merge policy: {status}");
            let (status, text) = run("POST", format!("/{}/_forcemerge?only_expunge_deletes=true", seg(index)), None).await?;
            anyhow::ensure!((200..300).contains(&status), "masking {index}: expunging what was masked: {status} {}", text.chars().take(300).collect::<String>());
        }
        Ok(())
    }

    async fn usage(&self, _parent: &str, branch: &str) -> anyhow::Result<Usage> {
        let c = self.0.get(&format!("/v1/search/clusters/{}", seg(branch))).await?;
        // What the branch wrote, when roll search says (`own_bytes`); else everything it
        // references, the parent's shared segments included (an overestimate).
        let bytes = c["own_bytes"].as_i64().or_else(|| c["stored_bytes"].as_i64()).unwrap_or(0);
        Ok(Usage { bytes, storage_usd_month: gb(bytes) * 0.023, active_usd_hour: 0.0, month_to_date_usd: 0.0 })
    }
}

#[cfg(test)]
mod tests {
    use super::bucket_name;

    /// A preview's search branch gives its services a data-only credential, never the branch's
    /// own login, and no link at all when roll search can't make one (or makes a wider one,
    /// revoked at once).
    #[tokio::test]
    async fn a_search_branch_gives_only_a_data_credential() {
        use axum::routing::{delete, get, post};
        use std::sync::{Arc, Mutex};
        let answer: Arc<Mutex<(u16, serde_json::Value)>> = Arc::new(Mutex::new((200, serde_json::json!({}))));
        let revoked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (a, r) = (answer.clone(), revoked.clone());
        let (r2, r3) = (revoked.clone(), revoked.clone());
        let app = axum::Router::new()
            .route("/v1/search/clusters/{id}", get(|| async { axum::Json(serde_json::json!({ "id": "srch_b", "name": "pv-x", "parent_id": "srch_p" })) }))
            .route("/v1/search/clusters/{id}/password", post(move || async move { r2.lock().unwrap().push("password".into()); axum::Json(serde_json::json!({})) }))
            .route("/v1/search/clusters/{id}", delete(move || async move { r3.lock().unwrap().push("cluster".into()); axum::http::StatusCode::NO_CONTENT }))
            .route(
                "/v1/search/clusters/{id}/keys",
                post(move || {
                    let a = a.clone();
                    async move {
                        let (code, body) = a.lock().unwrap().clone();
                        (axum::http::StatusCode::from_u16(code).unwrap(), axum::Json(body))
                    }
                }),
            )
            .route(
                "/v1/search/clusters/{id}/keys/{key}",
                delete(move |axum::extract::Path((_, key)): axum::extract::Path<(String, String)>| {
                    let r = r.clone();
                    async move {
                        r.lock().unwrap().push(key);
                        axum::http::StatusCode::NO_CONTENT
                    }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let search = super::Search(super::super::Api::new(addr));
        let branch = serde_json::json!({ "id": "srch_b", "url": "http://search.test:9200", "username": "srch_b", "password": "superuser-secret" });
        let kept = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        // A data key: the URL without the branch's login, the key beside it.
        *answer.lock().unwrap() = (200, serde_json::json!({ "id": "key_1", "access": "data", "api_key": "data-key" }));
        let held = roll_previews::names::search_link_holds_branch_login;
        let made = search.kept_with_key(&branch, "pv-x").await.unwrap();
        assert!(!held(&made), "never made again for it");
        assert_eq!(kept(&made), serde_json::json!({ "ELASTICSEARCH_URL": "http://search.test:9200", "ELASTICSEARCH_API_KEY": "data-key" }));
        // OpenSearch's data user: that user in the URL, never the branch's.
        *answer.lock().unwrap() = (200, serde_json::json!({ "id": "key_2", "access": "data", "username": "pv@srch_b", "password": "data-pass" }));
        let k = search.kept_with_key(&branch, "pv-x").await.unwrap();
        assert!(k.contains("pv%40srch_b:data-pass@") && !k.contains("superuser-secret") && !held(&k), "{k}");
        // Links made before (the branch's login in the URL) are: an open preview's branch is
        // made again, so the login stops working.
        assert!(held(r#"{"ELASTICSEARCH_URL":"http://srch_b:pw@search.test:9200"}"#));
        assert!(held(r#"{"ELASTICSEARCH_URL":"http://srch_b:pw@search.test:9200","ELASTICSEARCH_API_KEY":"data-key"}"#));
        assert!(held(r#"{"ELASTICSEARCH_URL":"http://srch_b:pw@search.test:9200","OPENSEARCH_USERNAME":"pv_data","OPENSEARCH_PASSWORD":"d"}"#));
        // None made: no link (the branch fails and is made again), never the login.
        *answer.lock().unwrap() = (500, serde_json::json!({ "error": "keys are off" }));
        assert!(search.kept_with_key(&branch, "pv-x").await.is_err());
        // A wider one: revoked at once, and no link.
        *answer.lock().unwrap() = (200, serde_json::json!({ "id": "key_3", "access": "admin", "api_key": "admin-key" }));
        assert!(search.kept_with_key(&branch, "pv-x").await.is_err());
        assert_eq!(revoked.lock().unwrap().clone(), vec!["key_3".to_string()]);
        // Deleted (or reset): roll search keeps it restorable, so the data key previews handed
        // out is revoked (by the id in it) and the branch's password changed before the delete.
        revoked.lock().unwrap().clear();
        use roll_previews::ports::Brancher as _;
        let key = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "key_1@pv-x:secret");
        search.retire("srch_p", "srch_b", Some(&serde_json::json!({ "ELASTICSEARCH_URL": "http://search.test:9200", "ELASTICSEARCH_API_KEY": key }).to_string())).await.unwrap();
        search.retire("srch_p", "srch_b", Some(&serde_json::json!({ "OPENSEARCH_USERNAME": "pv@pv-x" }).to_string())).await.unwrap();
        search.delete("srch_p", "srch_b", "pv-x").await.unwrap();
        assert_eq!(revoked.lock().unwrap().clone(), ["key_1", "pv", "password", "cluster"].map(String::from).to_vec());
    }

    /// Previews' masking over a real roll search (in process): on a branch of a seeded cluster,
    /// no original value is left in `_source`, `_terms_enum` or a terms aggregation on the
    /// `.keyword`, for a text field, an address inside an array of objects, a field under a dotted
    /// key and an index whose name has a dot; the parent keeps every original.
    /// A browsed table's name is quoted, never run: one with quotes stays one identifier.
    #[test]
    fn a_browsed_tables_name_is_quoted() {
        assert_eq!(super::browse_rows_sql("public.users").as_deref(), Some("SELECT * FROM \"public\".\"users\" LIMIT 51"));
        assert_eq!(super::browse_rows_sql("a\"b.c; DROP TABLE x").as_deref(), Some("SELECT * FROM \"a\"\"b\".\"c; DROP TABLE x\" LIMIT 51"));
        assert!(super::browse_rows_sql("users").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_masked_search_branch_shows_no_original_value() {
        use roll_previews::ports::Brancher as _;
        use serde_json::{Value, json};
        let home = tempfile::tempdir().unwrap();
        let mut cfg = search_control::SearchConfig::local(home.path().to_path_buf());
        cfg.gc_interval = None;
        cfg.unsealed_secrets = true;
        let es = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        cfg.http_port = es.local_addr().unwrap().port();
        let control = search_control::Control::new(cfg, search_store::test_objects("previews-mask")).await.unwrap();
        tokio::spawn(search_server::serve(es, control.node.clone(), search_server::ListenOptions::default()));
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = api.local_addr().unwrap();
        let router = search_control::api::router(control.clone());
        tokio::spawn(async move { axum::serve(api, router).await });
        let http = reqwest::Client::new();
        let post = |path: String, body: Value| {
            let req = http.post(format!("http://{addr}{path}")).json(&body);
            async move {
                let r = req.send().await.unwrap();
                let (status, v) = (r.status(), r.json::<Value>().await.unwrap());
                assert!(status.is_success(), "{path}: {status} {v}");
                v
            }
        };
        // One REST request on a cluster through the operator's console: its JSON body.
        let rest = |cluster: &str, method: &'static str, path: &str, body: Value| {
            let call = post(format!("/v1/search/clusters/{cluster}/request"), json!({ "method": method, "path": path, "body": body }));
            let path = path.to_string();
            async move {
                let v = call.await;
                assert!(v["status"].as_u64().is_some_and(|s| s < 300), "{method} {path}: {v}");
                serde_json::from_str::<Value>(v["body"].as_str().unwrap()).unwrap()
            }
        };
        post("/v1/search/clusters".into(), json!({ "name": "shop" })).await;
        let originals = ["Alice Original", "1 Secret St", "2 Hidden Rd", "alice@corp.example", "alice-secret-login"];
        rest(
            "shop",
            "PUT",
            "/people/_doc/1?refresh=true",
            json!({ "name": "Alice Original", "contacts": [{ "address": "1 Secret St" }, { "address": "2 Hidden Rd" }], "meta.email": "alice@corp.example" }),
        )
        .await;
        rest("shop", "PUT", "/logs.2024/_doc/1?refresh=true", json!({ "user": "alice-secret-login" })).await;
        let branch = post("/v1/search/clusters/shop/branches".into(), json!({ "name": "pv-mask" })).await["id"].as_str().unwrap().to_string();
        let fields: std::collections::BTreeMap<String, String> =
            [("people.name", "redact"), ("people.contacts.address", "redact"), ("people.meta.email", "email"), ("logs.2024.user", "redact")].map(|(k, v)| (k.to_string(), v.to_string())).into();
        super::Search(super::super::Api::new(addr)).mask("shop", &branch, None, &fields).await.unwrap();

        // Everything the branch can show of those fields, as one text.
        let shown = |cluster: String| async move {
            let mut out = vec![rest(&cluster, "POST", "/people,logs.2024/_search", json!({})).await];
            for (path, field) in [("/people/_terms_enum", "name.keyword"), ("/people/_terms_enum", "contacts.address.keyword"), ("/people/_terms_enum", "meta.email.keyword"), ("/logs.2024/_terms_enum", "user.keyword")] {
                out.push(rest(&cluster, "POST", path, json!({ "field": field })).await);
            }
            let aggs = json!({ "size": 0, "aggs": { "n": { "terms": { "field": "name.keyword" } }, "a": { "terms": { "field": "contacts.address.keyword" } }, "e": { "terms": { "field": "meta.email.keyword" } } } });
            out.push(rest(&cluster, "POST", "/people/_search", aggs).await);
            out.push(rest(&cluster, "POST", "/logs.2024/_search", json!({ "size": 0, "aggs": { "u": { "terms": { "field": "user.keyword" } } } })).await);
            Value::Array(out).to_string()
        };
        let masked = shown(branch.clone()).await;
        for o in originals {
            assert!(!masked.contains(o), "the branch still shows {o:?}: {masked}");
        }
        assert!(masked.contains("[masked]") && masked.contains("@example.invalid"), "{masked}");
        let parent = shown("shop".into()).await;
        for o in originals {
            assert!(parent.contains(o), "the parent lost {o:?}: {parent}");
        }
        control.shutdown().await.unwrap();
    }

    #[test]
    fn a_search_mapping_becomes_lines() {
        let v = serde_json::json!({
            ".kibana": { "mappings": { "properties": { "x": { "type": "text" } } } },
            "products": { "mappings": { "properties": {
                "name": { "type": "text", "fields": { "raw": { "type": "keyword" } } },
                "seller": { "properties": { "city": { "type": "text" } } }
            } } }
        });
        assert_eq!(
            super::mapping_lines(&v, &Default::default()),
            ["index products", "field products.name text", "field products.name.raw keyword", "field products.seller object", "field products.seller.city text"]
        );
    }

    #[test]
    fn bucket_branches_are_named_after_their_parent() {
        assert_eq!(bucket_name("assets", "pv-abc"), "assets-pv-abc");
        let long = "a".repeat(80);
        let n = bucket_name(&long, "pv-abcdefghijkl");
        assert_eq!(n.len(), 63);
        assert!(n.ends_with("-pv-abcdefghijkl"));
    }

    /// Every mask suggested for a real Postgres 18 schema's columns applies: masking with all of
    /// them at once ("Add all") succeeds, whatever holds the columns (generated and identity
    /// columns, a `varchar(11)` ssn, foreign keys both ways, arrays, a range, a CHECK, a domain,
    /// a unique expression, `NULLS NOT DISTINCT`, partition keys). Starts `postgres:18-alpine` in
    /// Docker and removes it: `cargo test -p rolld --bin rolld suggested_mask -- --ignored`.
    #[tokio::test]
    #[ignore = "needs Docker"]
    async fn every_suggested_mask_applies_on_real_postgres() {
        use std::process::Command;
        struct Container(String);
        impl Drop for Container {
            fn drop(&mut self) {
                let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
            }
        }
        let name = format!("roll-previews-masks-test-{}", std::process::id());
        let run = Command::new("docker").args(["run", "-d", "--rm", "--name", &name, "-e", "POSTGRES_PASSWORD=pw", "-p", "127.0.0.1::5432", "postgres:18-alpine"]).output().unwrap();
        assert!(run.status.success(), "docker run: {}", String::from_utf8_lossy(&run.stderr));
        let _container = Container(name.clone());
        let port = String::from_utf8(Command::new("docker").args(["port", &name, "5432"]).output().unwrap().stdout).unwrap();
        let port = port.lines().next().and_then(|l| l.rsplit(':').next()).unwrap().trim().to_string();
        let info = format!("host=127.0.0.1 port={port} user=postgres password=pw dbname=postgres");
        let mut client = None;
        for _ in 0..120 {
            if let Ok((c, conn)) = tokio_postgres::connect(&info, tokio_postgres::NoTls).await {
                tokio::spawn(conn);
                if c.simple_query("SELECT 1").await.is_ok() {
                    client = Some(c);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let pg = client.expect("postgres:18-alpine answers");
        pg.batch_execute(
            "CREATE TABLE taxes (id bigint PRIMARY KEY, tax_id text UNIQUE);
             CREATE DOMAIN us_zip AS text CHECK (VALUE ~ '^[0-9]{5}$');
             CREATE TABLE users (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               email varchar(255) NOT NULL UNIQUE,
               ssn varchar(11) NOT NULL,
               national_id char(9),
               phone_numbers varchar(20)[] NOT NULL DEFAULT '{}',
               phones bigint[],
               born daterange,
               first_name text NOT NULL CHECK (first_name <> '***'),
               last_name text NOT NULL,
               zip_code us_zip,
               tax_id bigint NOT NULL REFERENCES taxes,
               search_vector tsvector GENERATED ALWAYS AS (to_tsvector('simple', last_name)) STORED,
               full_name text GENERATED ALWAYS AS (first_name || ' ' || last_name) STORED,
               phone text,
               date_of_birth date NOT NULL);
             CREATE UNIQUE INDEX ON users (lower(last_name));
             CREATE TABLE contacts (id int PRIMARY KEY, email text, phone bigint NOT NULL, mobile text, UNIQUE NULLS NOT DISTINCT (mobile));
             CREATE TABLE visits (id int, client_ip inet NOT NULL, born_on date NOT NULL, home_address text) PARTITION BY RANGE (born_on);
             CREATE TABLE visits_old PARTITION OF visits FOR VALUES FROM ('2000-01-01') TO ('2100-01-01') PARTITION BY HASH (home_address);
             CREATE TABLE visits_old_0 PARTITION OF visits_old FOR VALUES WITH (MODULUS 1, REMAINDER 0);
             INSERT INTO taxes VALUES (1, 'T1'), (2, 'T2');
             INSERT INTO users (email, ssn, national_id, phone_numbers, phones, born, first_name, last_name, zip_code, tax_id, phone, date_of_birth) VALUES
               ('ann@corp.example', '123-45-6789', '123456789', '{555}', '{555}', '[1990-01-01,1990-01-02)', 'Ann', 'Lee', '12345', 1, '555', '1990-01-01'),
               ('bob@corp.example', '987-65-4321', NULL, '{}', NULL, NULL, 'Bob', 'Ray', NULL, 2, NULL, '1980-02-02');
             INSERT INTO contacts VALUES (1, 'ann@corp.example', 1, NULL), (2, NULL, 2, '07700');
             INSERT INTO visits VALUES (1, '10.0.0.1', '2001-01-01', '1 Main St'), (2, '10.0.0.2', '2002-01-01', NULL);",
        )
        .await
        .unwrap();

        let rows = pg.simple_query(&super::columns_sql(1000)).await.unwrap();
        let columns: Vec<_> = rows.iter().filter_map(|m| if let tokio_postgres::SimpleQueryMessage::Row(r) = m { super::column(r.get(0)?) } else { None }).collect();
        let suggested: std::collections::BTreeMap<String, String> =
            columns.iter().filter_map(|c| roll_previews::mask::suggest(roll_previews::Kind::Postgres, c).map(|x| (c.path.clone(), x.rule))).collect();
        let rule = |path: &str| suggested.get(path).map(String::as_str);
        // What it found and offered (so masking below isn't trivially all nulls).
        assert_eq!(rule("public.users.email"), Some("email"));
        assert_eq!(rule("public.users.ssn"), Some("redact"), "hash doesn't fit varchar(11)");
        assert_eq!(rule("public.users.phones"), Some("null"));
        assert_eq!(rule("public.contacts.mobile"), Some("hash"), "unique NULLS NOT DISTINCT: no null");
        assert_eq!(rule("public.visits.client_ip"), Some("fixed:0.0.0.0"));
        assert!(!columns.iter().any(|c| c.path == "public.users.full_name" || c.path == "public.users.id"), "generated and identity columns aren't offered");
        for held in ["users.tax_id", "users.first_name", "users.last_name", "users.zip_code", "users.phone_numbers", "visits.born_on", "visits.home_address", "taxes.id"] {
            assert_eq!(rule(&format!("public.{held}")), None, "{held}");
        }

        let plan = roll_previews::mask::postgres(&suggested, "salt").unwrap();
        pg.batch_execute(&plan.update).await.unwrap_or_else(|e| panic!("masking with every suggestion fails: {e:?}\n{suggested:?}"));
        for after in &plan.after {
            pg.batch_execute(after).await.unwrap();
        }
        let email: String = pg.query_one("SELECT email FROM public.users LIMIT 1", &[]).await.unwrap().get(0);
        assert!(email.starts_with("user-") && email.ends_with("@example.invalid"), "{email}");
    }
}
