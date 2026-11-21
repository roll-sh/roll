//! A branch's own role (`app_role`): the role code is handed instead of the project's superuser.
//! It owns the app's database, schemas and objects (migrations work) but isn't a superuser and
//! holds none of the roles that read files or other sessions' statistics, so it can't read the
//! branch's WAL (pages from before a mask, say) or another database. It's made on the branch,
//! after the branch point, so the parent's endpoint doesn't know it.

/// The loops that hand the objects `me` owns in the app's schemas to `role`: shared by a
/// branch's app role and the database's owner role.
macro_rules! move_owned {
    () => {
        r#"  FOR r IN SELECT n.nspname FROM pg_namespace n WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema' AND n.nspowner = me
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
"#
    };
}

/// Makes (or re-passwords) the role and gives it the app's objects: `{role}` and `{password}`
/// are quoted literals. Extension members, the system schemas, and sequences owned by a column
/// (they follow their table) are left as they are. Only the catalog's names resolve.
pub const ROLE_SQL: &str = concat!(
    r#"SET search_path = pg_catalog, pg_temp; DO $roll$
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
  -- What the database's owner role took over (SQL from a token that may only write) stays the
  -- app's to change: it's a member.
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'roll_owner') THEN
    EXECUTE format('GRANT roll_owner TO %I', role);
  END IF;
"#,
    move_owned!(),
    "END\n$roll$;"
);

/// The database's owner role (`roll_owner`, no login, never a superuser): it takes the
/// database and the objects the project's superuser owns in the app's schemas, so SQL from a
/// token that may only write runs as a member of it, never as the superuser (docs/mcp.md,
/// decision 6). Idempotent and cheap after the first run; run before each such query, it also
/// takes what was made since through the superuser's own connection string.
pub const OWNER_SQL: &str = concat!(
    r#"SET search_path = pg_catalog, pg_temp; DO $roll$
DECLARE
  r record;
  me oid := (SELECT oid FROM pg_roles WHERE rolname = current_user);
  role text := 'roll_owner';
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = role) THEN
    BEGIN
      EXECUTE format('CREATE ROLE %I NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS', role);
    EXCEPTION WHEN duplicate_object OR unique_violation THEN
      NULL; -- a query beside this one made it first
    END;
  END IF;
  IF (SELECT r2.rolsuper FROM pg_database d JOIN pg_roles r2 ON r2.oid = d.datdba WHERE d.datname = current_database()) THEN
    EXECUTE format('ALTER DATABASE %I OWNER TO %I', current_database(), role);
  END IF;
"#,
    move_owned!(),
    "END\n$roll$;"
);

/// The roles a writer joins: every role that owns the database or something in the app's schemas,
/// except a superuser, one that may make roles, databases or replication connections or bypass
/// row security, and one that is a member (by any path) of a superuser or of the roles that run
/// programs or read and write the server's files. Membership in those would pass on more than
/// ownership.
pub const OWNERS_SQL: &str = r#"SELECT DISTINCT o.rolname FROM (
    SELECT n.nspowner AS owner FROM pg_namespace n WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT c.relowner FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT p.proowner FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT t.typowner FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'
    UNION SELECT d.datdba FROM pg_database d WHERE d.datname = current_database()
) x JOIN pg_roles o ON o.oid = x.owner
WHERE NOT (o.rolsuper OR o.rolcreaterole OR o.rolcreatedb OR o.rolreplication OR o.rolbypassrls)
  AND o.rolname NOT LIKE 'roll\_wr\_%' AND o.rolname NOT LIKE 'roll\_ro\_%'
  AND NOT EXISTS (SELECT 1 FROM pg_roles s WHERE (s.rolsuper OR s.rolname IN ('pg_execute_server_program', 'pg_read_server_files', 'pg_write_server_files'))
                  AND s.oid <> o.oid AND pg_has_role(o.oid, s.oid, 'MEMBER'))"#;

/// `ROLE_SQL` for `role` with `password`.
pub fn role_sql(role: &str, password: &str) -> String {
    let quote = |v: &str| format!("'{}'", v.replace('\'', "''"));
    ROLE_SQL.replace("{role}", &quote(role)).replace("{password}", &quote(password))
}
