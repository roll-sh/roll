//! A branch's own role (`app_role`): the role code is handed instead of the project's superuser.
//! It owns the app's database, schemas and objects (migrations work) but isn't a superuser and
//! holds none of the roles that read files or other sessions' statistics, so it can't read the
//! branch's WAL (pages from before a mask, say) or another database. It's made on the branch,
//! after the branch point, so the parent's endpoint doesn't know it.

/// Makes (or re-passwords) the role and gives it the app's objects: `{role}` and `{password}`
/// are quoted literals. Extension members, the system schemas, and sequences owned by a column
/// (they follow their table) are left as they are. Only the catalog's names resolve.
pub const ROLE_SQL: &str = r#"SET search_path = pg_catalog, pg_temp; DO $roll$
DECLARE
  r record;
  me oid := (SELECT oid FROM pg_roles WHERE rolname = current_user);
  role text := {role};
  pw text := {password};
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = role) THEN
    EXECUTE format('ALTER ROLE %I LOGIN PASSWORD %L NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS', role, pw);
  ELSE
    EXECUTE format('CREATE ROLE %I LOGIN PASSWORD %L NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS', role, pw);
  END IF;
  EXECUTE format('ALTER DATABASE %I OWNER TO %I', current_database(), role);
  FOR r IN SELECT n.nspname FROM pg_namespace n WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema' AND n.nspowner = me
           AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_namespace'::regclass AND d.objid = n.oid AND d.deptype = 'e') LOOP
    EXECUTE format('ALTER SCHEMA %I OWNER TO %I', r.nspname, role);
  END LOOP;
  FOR r IN SELECT c.oid::regclass AS name, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema' AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') AND c.relowner = me
             AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND (d.deptype = 'e' OR (c.relkind = 'S' AND d.deptype IN ('a', 'i')))) LOOP
    EXECUTE format('ALTER %s %s OWNER TO %I', CASE r.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'S' THEN 'SEQUENCE' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'TABLE' END, r.name, role);
  END LOOP;
  FOR r IN SELECT p.oid::regprocedure AS name FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
           WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema' AND p.proowner = me
             AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e') LOOP
    EXECUTE format('ALTER ROUTINE %s OWNER TO %I', r.name, role);
  END LOOP;
  FOR r IN SELECT t.oid::regtype AS name FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
           WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema' AND t.typowner = me
             AND (t.typtype IN ('e', 'd', 'r') OR (t.typtype = 'c' AND (SELECT relkind FROM pg_class WHERE oid = t.typrelid) = 'c'))
             AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid AND d.deptype = 'e') LOOP
    EXECUTE format('ALTER TYPE %s OWNER TO %I', r.name, role);
  END LOOP;
END
$roll$;"#;

/// `ROLE_SQL` for `role` with `password`.
pub fn role_sql(role: &str, password: &str) -> String {
    let quote = |v: &str| format!("'{}'", v.replace('\'', "''"));
    ROLE_SQL.replace("{role}", &quote(role)).replace("{password}", &quote(password))
}
