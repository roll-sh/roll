//! Round 8 of the grammar (seeds from `R8_SEEDS` on), on round 7's shared engine with sessions
//! 2 and 3 over the wire: the language and type features earlier rounds left out.
//!
//! - PL/pgSQL: control flow (labelled loops, EXIT / CONTINUE, FOREACH ... SLICE, REVERSE),
//!   exceptions with SQLSTATEs, RAISE ... USING, GET [STACKED] DIAGNOSTICS, ASSERT, CASE
//!   without ELSE, STRICT INTO, dynamic SQL, cursors (bound, unbound, scroll, refcursors),
//!   RETURN NEXT / QUERY / QUERY EXECUTE, procedures with transaction control, OUT / INOUT /
//!   VARIADIC / DEFAULT parameters.
//! - Triggers of every kind: BEFORE / AFTER / INSTEAD OF, row and statement, UPDATE OF, WHEN,
//!   TRUNCATE, transition tables, constraint triggers deferred to COMMIT, TG_* variables; event
//!   triggers (ddl_command_start / end, sql_drop, table_rewrite).
//! - Window functions with every frame mode, offsets and EXCLUDE; GROUPING SETS / CUBE /
//!   ROLLUP; recursive CTEs with SEARCH and CYCLE over a graph with cycles.
//! - SQL/JSON (JSON_TABLE with NESTED paths, JSON_QUERY / JSON_VALUE / JSON_EXISTS behaviours,
//!   constructors, IS JSON) and jsonpath edge cases; ranges and multiranges; text search;
//!   large objects; COPY options (ON_ERROR, REJECT_LIMIT, DEFAULT, HEADER MATCH, FORCE_*,
//!   WHERE, FREEZE); LISTEN / NOTIFY across sessions; prepared transactions; queries under a
//!   small work_mem that spill (sort, hash join, hash aggregate) with a total order.
//!
//! Everything is deterministic by construction: row-trigger logs are read in order only after
//! single-row statements, frames whose rows tie read order-insensitive aggregates, ORDER BY
//! covers every output column where `ordered` is set. Objects are named `r8*`; functions,
//! triggers and channels made by a statement group carry a serial so a case stands alone.

use super::{Query, Rng, Stmt, Ty};

fn q(sql: impl Into<String>, ordered: bool) -> Stmt {
    Stmt::Query(Query { sql: sql.into(), ordered, cols: Vec::<(String, Ty)>::new() })
}
fn dml(sql: impl Into<String>, table: &str) -> Stmt {
    Stmt::Dml { sql: sql.into(), table: table.into(), ordered: false }
}
fn o(s: impl Into<String>) -> Stmt {
    Stmt::Other(s.into())
}

const WORDS: &[&str] = &[
    "the quick brown fox", "jumps over the lazy dog", "foxes and dogs", "a rat ran", "running rats",
    "quick quick slow", "", "dog-eat-dog world", "brown's fox's den", "über café naïve",
];

pub fn schema(rng: &mut Rng) -> Vec<String> {
    let mut out: Vec<String> = vec![
        "CREATE TABLE r8a (id int PRIMARY KEY, g int, v int, x numeric(8, 2), t text, ts timestamp)".into(),
        "CREATE TABLE r8log (n serial PRIMARY KEY, tg text, info text)".into(),
    ];
    let mut rows = Vec::new();
    for id in 1..=rng.range(6, 18) {
        let g = rng.range(1, 4);
        let v = if rng.chance(0.15) { "NULL".to_string() } else { rng.range(-5, 20).to_string() };
        let x = *rng.pick(&["1.50", "-2.25", "0", "NULL", "10", "3.75", "1.50"]);
        let t = *rng.pick(&["'a'", "'b'", "'B'", "NULL", "'ab'", "''", "'z'"]);
        let ts = format!("'2025-01-01'::timestamp + interval '{} hours'", rng.range(0, 72) * rng.range(1, 3));
        rows.push(format!("({id}, {g}, {v}, {x}, {t}, {ts})"));
    }
    out.push(format!("INSERT INTO r8a VALUES {}", rows.join(", ")));
    // A graph with cycles for SEARCH / CYCLE.
    out.push("CREATE TABLE r8edge (src int, dst int, w int)".into());
    let n = rng.range(4, 8);
    let mut edges = Vec::new();
    for s in 1..=n {
        for _ in 0..rng.range(0, 2) {
            let d = rng.range(1, n);
            edges.push(format!("({s}, {d}, {})", rng.range(1, 9)));
        }
    }
    edges.push("(1, 2, 1)".into());
    out.push(format!("INSERT INTO r8edge VALUES {}", edges.join(", ")));
    // Ranges and multiranges.
    out.push("CREATE TABLE r8rng (id int PRIMARY KEY, r int4range, mr int4multirange, d daterange, n numrange)".into());
    let mut rows = Vec::new();
    for id in 1..=rng.range(4, 10) {
        let a = rng.range(-3, 10);
        let b = a + rng.range(0, 6);
        let r = rng.pick(&[format!("'[{a},{b})'"), format!("'({a},{b}]'"), "'empty'".into(), format!("'[{a},)'"), "NULL".into()]).clone();
        let mr = rng.pick(&[format!("'{{[{a},{b}), [{}, {})}}'", b + 2, b + 4), "'{}'".into(), format!("'{{(,{a})}}'"), "NULL".into()]).clone();
        let d = *rng.pick(&["'[2025-01-01,2025-02-01)'", "'(2024-12-31,2025-01-05]'", "'[2025-03-01,infinity)'", "'empty'", "NULL"]);
        let nr = *rng.pick(&["'[1.5,2.5)'", "'(,0]'", "'[0,0]'", "NULL", "'(1.25,1.5)'"]);
        rows.push(format!("({id}, {r}, {mr}, {d}, {nr})"));
    }
    out.push(format!("INSERT INTO r8rng VALUES {}", rows.join(", ")));
    // Text search: a stored tsvector and a GIN index.
    out.push("CREATE TABLE r8doc (id int PRIMARY KEY, body text, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED)".into());
    let rows: Vec<String> = (1..=rng.range(4, 10)).map(|i| format!("({i}, '{}')", rng.pick(WORDS).replace('\'', "''"))).collect();
    out.push(format!("INSERT INTO r8doc (id, body) VALUES {}", rows.join(", ")));
    if rng.chance(0.5) {
        out.push("CREATE INDEX r8doc_tsv ON r8doc USING gin (tsv)".into());
    }
    // JSON documents.
    out.push("CREATE TABLE r8j (id int PRIMARY KEY, j jsonb)".into());
    let docs = [
        r#"{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}"#,
        r#"{"a": "2", "b": [], "s": "2025-03-09T02:30:00"}"#,
        r#"[1, "two", 3.5, null, true, {"k": [10, 20]}]"#,
        r#"{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}"#,
        r#""scalar""#,
        r#"{"a": 1e3, "n": -0.0, "big": 12345678901234567890}"#,
    ];
    let rows: Vec<String> = (1..=rng.range(3, 6)).map(|i| format!("({i}, '{}')", rng.pick(&docs))).collect();
    out.push(format!("INSERT INTO r8j VALUES {}", rows.join(", ")));
    // A bigger table for queries that spill under a small work_mem (unique keys: total orders).
    let m = rng.range(2000, 4000);
    out.push("CREATE TABLE r8big (id int PRIMARY KEY, k int, s text)".into());
    out.push(format!("INSERT INTO r8big SELECT i, (i * 7919) % 97, md5(i::text) FROM generate_series(1, {m}) i"));
    if rng.chance(0.6) {
        for t in ["r8a", "r8edge", "r8rng", "r8doc", "r8j", "r8big"] {
            out.push(format!("ANALYZE {t}"));
        }
    }
    out
}

/// PL/pgSQL: a function of one of several shapes made, called with a few arguments, dropped.
fn plpgsql(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let f = format!("r8f{k}");
    let mut out = Vec::new();
    let (def, calls): (String, Vec<String>) = match rng.below(10) {
        0 => {
            // Labelled loops, EXIT / CONTINUE, REVERSE and BY.
            let by = rng.range(1, 3);
            (format!("CREATE FUNCTION {f}(a int, b int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text := ''; i int; j int;
BEGIN
  <<outer>> FOR i IN REVERSE a..b BY {by} LOOP
    CONTINUE WHEN i % 3 = 0;
    j := 0;
    <<inner>> LOOP
      j := j + 1;
      EXIT outer WHEN length(r) > 40;
      EXIT inner WHEN j >= i;
      CONTINUE outer WHEN j = 4;
      r := r || i || ':' || j || ' ';
    END LOOP inner;
    WHILE j > 0 AND i < 0 LOOP j := j - 1; r := r || 'w'; END LOOP;
  END LOOP outer;
  RETURN r;
END $$"), vec![format!("SELECT {f}(9, 1)"), format!("SELECT {f}({}, {})", rng.range(-3, 8), rng.range(-3, 2)), format!("SELECT {f}(NULL, 1)")])
        }
        1 => {
            // FOREACH with and without SLICE.
            let slice = rng.range(0, 2);
            let decl = if slice == 0 { "int" } else { "int[]" };
            (format!("CREATE FUNCTION {f}(a int[]) RETURNS SETOF text LANGUAGE plpgsql AS $$
DECLARE x {decl};
BEGIN
  FOREACH x SLICE {slice} IN ARRAY a LOOP RETURN NEXT x::text; END LOOP;
  RETURN;
END $$"), vec![format!("SELECT * FROM {f}('{{{{1,2}},{{3,4}}}}')"), format!("SELECT * FROM {f}('{{5,NULL,7}}')"), format!("SELECT * FROM {f}('{{}}')"), format!("SELECT * FROM {f}(NULL)")])
        }
        2 => {
            // An error raised, caught by its condition, and its fields read back.
            let raise = *rng.pick(&[
                "PERFORM 1 / $1",
                "RAISE EXCEPTION 'custom %', $1 USING ERRCODE = 'R8001', DETAIL = 'd' || $1, HINT = 'h'",
                "RAISE SQLSTATE '22012' USING MESSAGE = 'by hand'",
                "RAISE unique_violation USING MESSAGE = 'dup ' || $1, COLUMN = 'c', CONSTRAINT = 'k', TABLE = 't', SCHEMA = 's'",
                "INSERT INTO r8a (id) VALUES (1), (1)",
                "PERFORM ('{1,2}'::int[])[$1:$1]::int",
                "ASSERT $1 > 0, 'assert ' || $1",
                "PERFORM jsonb_path_query_first('[1]', 'strict $[$x]', jsonb_build_object('x', $1))",
            ]);
            let handler = rng.pick(&["division_by_zero OR unique_violation", "SQLSTATE 'R8001'", "data_exception", "OTHERS", "assert_failure", "integrity_constraint_violation"]);
            (format!("CREATE FUNCTION {f}(int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE st text; msg text; det text; hnt text; col text; con text; ctx text;
BEGIN
  BEGIN
    {raise};
    RETURN 'no error';
  EXCEPTION WHEN {handler} THEN
    GET STACKED DIAGNOSTICS st = RETURNED_SQLSTATE, msg = MESSAGE_TEXT, det = PG_EXCEPTION_DETAIL,
      hnt = PG_EXCEPTION_HINT, col = COLUMN_NAME, con = CONSTRAINT_NAME, ctx = PG_EXCEPTION_CONTEXT;
    RETURN concat_ws('|', st, msg, det, hnt, col, con, ctx IS NOT NULL);
  END;
END $$"), vec![format!("SELECT {f}(0)"), format!("SELECT {f}(1)"), format!("SELECT {f}({})", rng.range(-2, 3))])
        }
        3 => {
            // ROW_COUNT, FOUND, STRICT INTO and its errors.
            let strict = if rng.chance(0.6) { " STRICT" } else { "" };
            (format!("CREATE FUNCTION {f}(lo int, hi int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE n bigint; r text := ''; got int;
BEGIN
  UPDATE r8a SET v = v WHERE id BETWEEN lo AND hi;
  GET DIAGNOSTICS n = ROW_COUNT;
  r := r || 'upd=' || n || ' found=' || FOUND;
  SELECT id INTO{strict} got FROM r8a WHERE id BETWEEN lo AND hi ORDER BY id;
  r := r || ' got=' || coalesce(got::text, 'null') || ' found=' || FOUND;
  PERFORM 1 FROM r8a WHERE false;
  r := r || ' perform=' || FOUND;
  RETURN r;
END $$"), vec![format!("SELECT {f}(1, 1)"), format!("SELECT {f}(1, 3)"), format!("SELECT {f}(1000, 1001)")])
        }
        4 => {
            // Explicit cursors: bound with arguments, SCROLL with FETCH / MOVE directions.
            let dir = rng.pick(&["PRIOR", "FIRST", "LAST", "ABSOLUTE 2", "RELATIVE -1", "NEXT", "ABSOLUTE -1", "RELATIVE 0"]);
            (format!("CREATE FUNCTION {f}(lim int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE c SCROLL CURSOR (m int) FOR SELECT id FROM r8a WHERE id <= m ORDER BY id; v int; r text := '';
BEGIN
  OPEN c(lim);
  LOOP FETCH c INTO v; EXIT WHEN NOT FOUND; r := r || v || ','; END LOOP;
  FETCH {dir} FROM c INTO v;
  r := r || ' {dir}=' || coalesce(v::text, '-') || ' ' || FOUND;
  MOVE BACKWARD 2 FROM c;
  FETCH c INTO v;
  r := r || ' back=' || coalesce(v::text, '-');
  CLOSE c;
  RETURN r;
END $$"), vec![format!("SELECT {f}(4)"), format!("SELECT {f}(0)"), format!("SELECT {f}({})", rng.range(1, 20))])
        }
        5 => {
            // A refcursor returned to the caller, then fetched in the transaction.
            let named = rng.chance(0.5);
            let name = if named { format!("'r8cur{k}'") } else { "NULL".into() };
            out.push(o(format!("CREATE FUNCTION {f}(m int) RETURNS refcursor LANGUAGE plpgsql AS $$
DECLARE c refcursor := {name};
BEGIN OPEN c FOR EXECUTE format('SELECT id, g FROM r8a WHERE id <= %s ORDER BY id', m); RETURN c; END $$")));
            out.push(o("BEGIN"));
            out.push(o(format!("SELECT {f}(3) IS NOT NULL")));
            if named {
                out.push(o(format!("FETCH 2 FROM r8cur{k}")));
                out.push(o(format!("FETCH ALL FROM r8cur{k}")));
            }
            out.push(o("SELECT name LIKE '<unnamed portal%' AS unnamed, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY name"));
            out.push(o("COMMIT"));
            (String::new(), vec![])
        }
        6 => {
            // RETURN QUERY / RETURN QUERY EXECUTE ... USING / RETURN NEXT with OUT parameters.
            (format!("CREATE FUNCTION {f}(m int, OUT a int, OUT b text) RETURNS SETOF record LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT id, t FROM r8a WHERE id <= m ORDER BY id;
  RETURN QUERY EXECUTE 'SELECT $1 * 100, $2' USING m, 'x';
  a := -1; b := NULL; RETURN NEXT;
  IF m < 0 THEN RETURN QUERY SELECT 1, 2; END IF;
END $$"), vec![format!("SELECT * FROM {f}(2)"), format!("SELECT a, b FROM {f}({}) ORDER BY a, b", rng.range(-1, 5)), format!("SELECT * FROM {f}(-1)")])
        }
        7 => {
            // CASE without ELSE, dynamic SQL with format() and quote_*.
            (format!("CREATE FUNCTION {f}(x int) RETURNS text LANGUAGE plpgsql AS $$
DECLARE r text; n int;
BEGIN
  CASE x WHEN 1, 2 THEN r := 'low'; WHEN 3 THEN r := 'three'; END CASE;
  EXECUTE format('SELECT count(*) FROM %I WHERE id > %L', 'r8a', x) INTO n;
  RETURN r || n || quote_nullable(NULL) || quote_literal('it''s');
END $$"), vec![format!("SELECT {f}(1)"), format!("SELECT {f}(3)"), format!("SELECT {f}({})", rng.range(0, 5))])
        }
        8 => {
            // VARIADIC and DEFAULT parameters, named notation.
            (format!("CREATE FUNCTION {f}(a int, b int DEFAULT 10, VARIADIC c int[] DEFAULT '{{}}') RETURNS text LANGUAGE plpgsql AS $$
BEGIN RETURN a || '/' || b || '/' || coalesce(array_length(c, 1), 0) || '/' || coalesce(c[1], -1); END $$"), vec![
                format!("SELECT {f}(1)"), format!("SELECT {f}(1, 2, 3, 4)"), format!("SELECT {f}(b => 5, a => 1)"),
                format!("SELECT {f}(1, 2, VARIADIC '{{7,8}}')"), format!("SELECT {f}(1, c => '{{}}')"), format!("SELECT {f}(1, 2, VARIADIC NULL)"),
            ])
        }
        _ => {
            // A procedure with transaction control and an INOUT parameter.
            let end = rng.pick(&["COMMIT", "ROLLBACK", "COMMIT AND CHAIN"]);
            out.push(o(format!("CREATE PROCEDURE {f}(INOUT n int, m int) LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('proc', 'first ' || m);
  {end};
  INSERT INTO r8log (tg, info) VALUES ('proc', 'second ' || m);
  n := n + m;
END $$")));
            out.push(o(format!("CALL {f}(1, 2)")));
            out.push(o("BEGIN"));
            out.push(o(format!("CALL {f}(5, 6)")));
            out.push(o("ROLLBACK"));
            out.push(o(format!("DO $$ DECLARE x int := 0; BEGIN CALL {f}(x, 3); RAISE NOTICE 'x=%', x; END $$")));
            out.push(q("SELECT tg, info FROM r8log WHERE tg = 'proc' ORDER BY n", true));
            out.push(o(format!("DROP PROCEDURE {f}")));
            return out;
        }
    };
    if !def.is_empty() {
        out.push(o(def));
    }
    out.extend(calls.into_iter().map(o));
    out.push(o(format!("DROP FUNCTION {f}")));
    out
}

/// Triggers of every kind on a table of the group's own, each firing logged into r8log; the
/// log is read in order after statements that touch one row, as a multiset otherwise.
fn triggers(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let (t, f, tag) = (format!("r8t{k}"), format!("r8tf{k}"), format!("t{k}"));
    let mut out = vec![
        o(format!("CREATE TABLE {t} (id int PRIMARY KEY, v int, s text)")),
        o(format!("INSERT INTO {t} VALUES (1, 10, 'a'), (2, 20, 'b'), (3, -3, NULL)")),
        o(format!("CREATE FUNCTION {f}() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO r8log (tg, info) VALUES ('{tag}', concat_ws(' ', TG_NAME, TG_WHEN, TG_LEVEL, TG_OP, TG_TABLE_NAME, TG_NARGS, TG_ARGV[0],
    CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'INSERT' THEN OLD::text END, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW::text END));
  IF TG_LEVEL = 'STATEMENT' OR TG_WHEN = 'AFTER' THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'skip' AND TG_OP <> 'DELETE' AND NEW.v < 0 THEN RETURN NULL; END IF;
  IF TG_ARGV[0] = 'mod' AND TG_OP <> 'DELETE' THEN NEW.v := NEW.v * 10; NEW.s := coalesce(NEW.s, '?') || '!'; END IF;
  IF TG_ARGV[0] = 'fail' AND TG_OP <> 'DELETE' AND NEW.v = 0 THEN RAISE EXCEPTION 'zero v in %', TG_NAME USING ERRCODE = 'R8002'; END IF;
  IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$")),
        o(format!("CREATE FUNCTION {f}_tt() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r text := TG_NAME || ' ' || TG_OP;
BEGIN
  -- (Apart: a transition table the trigger doesn't have fails the statement naming it.)
  IF TG_OP IN ('UPDATE', 'DELETE') THEN
    r := r || ' old=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM ot);
  END IF;
  IF TG_OP IN ('UPDATE', 'INSERT') THEN
    r := r || ' new=' || (SELECT coalesce(string_agg(id || ':' || coalesce(v::text, '-'), ',' ORDER BY id), '') FROM nt);
  END IF;
  INSERT INTO r8log (tg, info) VALUES ('{tag}', r);
  RETURN NULL;
END $$")),
    ];
    let kinds = [
        format!("CREATE TRIGGER a_bir BEFORE INSERT OR UPDATE ON {t} FOR EACH ROW EXECUTE FUNCTION {f}('{}')", rng.pick(&["mod", "skip", "fail", "none"])),
        format!("CREATE TRIGGER b_air AFTER INSERT OR DELETE ON {t} FOR EACH ROW EXECUTE FUNCTION {f}()"),
        format!("CREATE TRIGGER c_bus BEFORE UPDATE OR DELETE ON {t} FOR EACH STATEMENT EXECUTE FUNCTION {f}('stmt')"),
        format!("CREATE TRIGGER d_auof AFTER UPDATE OF v ON {t} FOR EACH ROW WHEN (OLD.v IS DISTINCT FROM NEW.v) EXECUTE FUNCTION {f}('of')"),
        format!("CREATE TRIGGER e_tr BEFORE TRUNCATE ON {t} FOR EACH STATEMENT EXECUTE FUNCTION {f}()"),
        format!("CREATE TRIGGER f_tt AFTER UPDATE ON {t} REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION {f}_tt()"),
        format!("CREATE TRIGGER g_ti AFTER INSERT ON {t} REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION {f}_tt()"),
        format!("CREATE CONSTRAINT TRIGGER h_def AFTER INSERT OR UPDATE ON {t} DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {f}('deferred')"),
        format!("CREATE TRIGGER i_td AFTER DELETE ON {t} REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION {f}_tt()"),
        format!("CREATE TRIGGER j_bad AFTER INSERT ON {t} REFERENCING OLD TABLE AS ot FOR EACH ROW EXECUTE FUNCTION {f}_tt()"),
        format!("CREATE TRIGGER k_bad BEFORE UPDATE ON {t} REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION {f}_tt()"),
    ];
    for _ in 0..rng.range(2, 5) {
        out.push(o(rng.pick(&kinds).clone()));
    }
    let log = |out: &mut Vec<Stmt>, ordered: bool| {
        out.push(q(format!("SELECT info FROM r8log WHERE tg = '{tag}'{}", if ordered { " ORDER BY n" } else { "" }), ordered));
        out.push(o(format!("DELETE FROM r8log WHERE tg = '{tag}'")));
    };
    let v = rng.range(-2, 5);
    // (One row at a time: the firing order is fixed.)
    for (sql, one) in [
        (format!("INSERT INTO {t} VALUES (10, {v}, 'n') RETURNING *"), true),
        (format!("UPDATE {t} SET v = {} WHERE id = 1 RETURNING id, v, s", rng.pick(&["v + 1", "v", "0", "NULL"])), true),
        (format!("INSERT INTO {t} VALUES (2, 7, 'c') ON CONFLICT (id) DO UPDATE SET v = excluded.v + {t}.v RETURNING *"), true),
        (format!("DELETE FROM {t} WHERE id = 3 RETURNING *"), true),
        (format!("UPDATE {t} SET v = coalesce(v, 0) - 1 RETURNING id, v"), false),
        (format!("INSERT INTO {t} SELECT g, g - 22, 'x' FROM generate_series(20, 23) g"), false),
    ] {
        if rng.chance(0.75) {
            out.push(dml(sql, &t));
            log(&mut out, one);
        }
    }
    if rng.chance(0.5) {
        out.push(o("BEGIN"));
        out.push(dml(format!("UPDATE {t} SET s = 'deferred' WHERE id = 2"), &t));
        out.push(q(format!("SELECT count(*) FROM r8log WHERE tg = '{tag}'"), false));
        out.push(o(rng.pick(&["COMMIT", "ROLLBACK", "SET CONSTRAINTS ALL IMMEDIATE"]).to_string()));
        out.push(o("COMMIT"));
        log(&mut out, true);
    }
    if rng.chance(0.4) {
        out.push(o(format!("TRUNCATE {t}")));
        log(&mut out, true);
    }
    out.push(q(format!("SELECT tgname, tgtype, tgenabled, tgdeferrable, tginitdeferred, tgnargs, tgoldtable, tgnewtable FROM pg_trigger WHERE tgrelid = '{t}'::regclass ORDER BY tgname"), true));
    out.push(o(format!("DROP TABLE {t}")));
    out.push(o(format!("DROP FUNCTION {f}, {f}_tt")));
    out
}

/// INSTEAD OF triggers on a view, and event triggers around DDL.
fn view_and_event_triggers(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let mut out = Vec::new();
    if rng.chance(0.5) {
        let (vw, f) = (format!("r8v{k}"), format!("r8vf{k}"));
        out.push(o(format!("CREATE VIEW {vw} AS SELECT id, v, upper(t) AS ut FROM r8a WHERE id > 100")));
        out.push(o(format!("CREATE FUNCTION {f}() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN INSERT INTO r8a (id, v, t) VALUES (NEW.id, NEW.v, lower(NEW.ut)); RETURN NEW;
  ELSIF TG_OP = 'UPDATE' THEN UPDATE r8a SET v = NEW.v WHERE id = OLD.id; RETURN {};
  ELSE DELETE FROM r8a WHERE id = OLD.id; RETURN OLD; END IF;
END $$", rng.pick(&["NEW", "NULL", "OLD"]))));
        out.push(o(format!("CREATE TRIGGER r8vt INSTEAD OF INSERT OR UPDATE OR DELETE ON {vw} FOR EACH ROW EXECUTE FUNCTION {f}()")));
        let id = 100 + k % 1000 + 1;
        out.push(dml(format!("INSERT INTO {vw} VALUES ({id}, 5, 'Q') RETURNING *"), "r8a"));
        out.push(dml(format!("UPDATE {vw} SET v = v + 1 WHERE id = {id} RETURNING *"), "r8a"));
        out.push(dml(format!("DELETE FROM {vw} WHERE id = {id} RETURNING id, ut"), "r8a"));
        out.push(o(format!("DROP VIEW {vw}")));
        out.push(o(format!("DROP FUNCTION {f}")));
        return out;
    }
    let (f, e) = (format!("r8ef{k}"), format!("r8e{k}"));
    let event = *rng.pick(&["ddl_command_start", "ddl_command_end", "sql_drop", "table_rewrite"]);
    let body = match event {
        "ddl_command_end" => "RAISE NOTICE 'end % %', tg_tag, (SELECT string_agg(command_tag || ':' || object_type || ':' || coalesce(object_identity, '-') || ':' || in_extension, ', ' ORDER BY object_identity, command_tag) FROM pg_event_trigger_ddl_commands())",
        "sql_drop" => "RAISE NOTICE 'drop % %', tg_tag, (SELECT string_agg(object_type || ':' || coalesce(object_identity, '-') || ':' || original || ':' || normal || ':' || is_temporary, ', ' ORDER BY object_type, object_identity) FROM pg_event_trigger_dropped_objects() WHERE schema_name IS DISTINCT FROM 'pg_toast')",
        "table_rewrite" => "RAISE NOTICE 'rewrite % % %', tg_tag, pg_event_trigger_table_rewrite_oid()::regclass, pg_event_trigger_table_rewrite_reason()",
        _ => "RAISE NOTICE 'start % %', tg_event, tg_tag; IF tg_tag = 'COMMENT' THEN RAISE EXCEPTION 'no comments' USING ERRCODE = 'R8003'; END IF",
    };
    out.push(o(format!("CREATE FUNCTION {f}() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN {body}; END $$")));
    let when = if rng.chance(0.3) { " WHEN TAG IN ('CREATE TABLE', 'DROP TABLE', 'ALTER TABLE', 'COMMENT')" } else { "" };
    out.push(o(format!("CREATE EVENT TRIGGER {f} ON {event}{when} EXECUTE FUNCTION {f}()")));
    for s in [
        format!("CREATE TABLE {e} (a int PRIMARY KEY, b text)"),
        format!("CREATE INDEX {e}_b ON {e} (b)"),
        format!("ALTER TABLE {e} ADD COLUMN c int DEFAULT 1"),
        format!("ALTER TABLE {e} ALTER COLUMN a TYPE bigint"),
        format!("ALTER TABLE {e} ADD COLUMN d timestamptz DEFAULT clock_timestamp()"),
        format!("COMMENT ON TABLE {e} IS 'x'"),
        format!("CREATE VIEW {e}_v AS SELECT a FROM {e}"),
        format!("ALTER TABLE {e} DROP COLUMN b"),
        format!("DROP TABLE {e} CASCADE"),
    ] {
        if rng.chance(0.8) || s.starts_with("CREATE TABLE") || s.starts_with("DROP") {
            out.push(o(s));
        }
    }
    out.push(o(format!("SELECT evtname, evtevent, evtenabled, evttags FROM pg_event_trigger WHERE evtname = '{f}'")));
    out.push(o(format!("DROP EVENT TRIGGER {f}")));
    out.push(o(format!("DROP FUNCTION {f}")));
    out
}

/// A window function over r8a under one frame; rows that tie under the frame's ORDER BY only
/// meet order-insensitive aggregates, or value functions of the key itself.
fn windows(rng: &mut Rng) -> Vec<Stmt> {
    let mode = *rng.pick(&["ROWS", "RANGE", "GROUPS"]);
    // (A total order for ROWS; one key, with ties, for RANGE and GROUPS.)
    let (order, key, off) = match (mode, rng.below(5)) {
        ("ROWS", _) => ("g, id", "g", rng.range(0, 3).to_string()),
        (_, 0) => ("ts", "ts", rng.pick(&["'12 hours'", "'1 day'", "'0'", "'-1 hour'"]).to_string()),
        (_, 1) => ("x", "x", rng.pick(&["1.5", "0", "2", "NULL"]).to_string()),
        (_, 2) => ("v DESC NULLS LAST", "v", rng.range(0, 4).to_string()),
        (_, 3) => ("id DESC", "id", rng.pick(&["1", "3", "-1"]).to_string()),
        _ => ("g", "g", rng.range(0, 2).to_string()),
    };
    let off = if mode == "GROUPS" && key != "g" { rng.range(0, 2).to_string() } else { off };
    let bound = |rng: &mut Rng, start: bool| -> String {
        match rng.below(5) {
            0 if start => "UNBOUNDED PRECEDING".into(),
            0 => "UNBOUNDED FOLLOWING".into(),
            1 => format!("{off} PRECEDING"),
            2 => "CURRENT ROW".into(),
            3 => format!("{off} FOLLOWING"),
            _ if start => format!("{off} PRECEDING"),
            _ => format!("{off} FOLLOWING"),
        }
    };
    let (a, b) = (bound(rng, true), bound(rng, false));
    let exclude = *rng.pick(&["", "", " EXCLUDE CURRENT ROW", " EXCLUDE GROUP", " EXCLUDE TIES", " EXCLUDE NO OTHERS"]);
    let part = *rng.pick(&["", "", "PARTITION BY g % 2 ", "PARTITION BY t "]);
    let frame = if rng.chance(0.15) { format!("{mode} {a}{exclude}") } else { format!("{mode} BETWEEN {a} AND {b}{exclude}") };
    let funcs: Vec<String> = if mode == "ROWS" {
        ["sum(v)", "array_agg(id)", "first_value(t)", "last_value(v)", "nth_value(id, 2)", "string_agg(t, ',')", "count(*) FILTER (WHERE v > 0)", "avg(x)"].map(String::from).to_vec()
    } else {
        let mut v = ["sum(v)", "count(*)", "max(t)", "min(x)", "bool_or(v > 3)", "count(v) FILTER (WHERE g > 1)"].map(String::from).to_vec();
        v.extend([format!("first_value({key})"), format!("last_value({key})"), format!("nth_value({key}, 2)")]);
        v
    };
    let f1 = rng.pick(&funcs).clone();
    let f2 = rng.pick(&funcs).clone();
    let mut out = vec![q(
        format!("SELECT id, {f1} OVER w AS w1, {f2} OVER (w2 {frame}) AS w2 FROM r8a WINDOW w AS ({part}ORDER BY {order} {frame}), w2 AS ({part}ORDER BY {order}) ORDER BY id"),
        true,
    )];
    if rng.chance(0.3) {
        // Ranking and offsets over a total order, and the frame errors.
        out.push(q(format!("SELECT id, row_number() OVER w, rank() OVER w, dense_rank() OVER w, percent_rank() OVER w, cume_dist() OVER w, ntile({}) OVER w, lag(v, {}, -1) OVER w, lead(t) OVER w FROM r8a WINDOW w AS (PARTITION BY g ORDER BY v NULLS FIRST, id) ORDER BY id", rng.range(1, 4), rng.range(0, 2)), true));
        out.push(o(*rng.pick(&[
            "SELECT sum(v) OVER (ORDER BY g, id RANGE 1 PRECEDING) FROM r8a",
            "SELECT sum(v) OVER (GROUPS 1 PRECEDING) FROM r8a",
            "SELECT sum(v) OVER (ORDER BY t RANGE 1 PRECEDING) FROM r8a",
            "SELECT sum(v) OVER (ORDER BY g ROWS -1 PRECEDING) FROM r8a",
            "SELECT sum(v) OVER (ORDER BY g ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM r8a",
            "SELECT sum(v) OVER (ORDER BY g ROWS UNBOUNDED FOLLOWING) FROM r8a",
            "SELECT sum(v) OVER (w ORDER BY id) FROM r8a WINDOW w AS (ORDER BY g)",
            "SELECT sum(v) OVER (w) FROM r8a WINDOW w AS (ORDER BY g ROWS 1 PRECEDING), w AS (ORDER BY id)",
            "SELECT sum(v) OVER (w RANGE 1 PRECEDING) FROM r8a WINDOW w AS (ORDER BY g ROWS 1 PRECEDING)",
            "SELECT sum(v) OVER (ORDER BY g ROWS v PRECEDING) FROM r8a",
            "SELECT sum(v) OVER (ORDER BY ts RANGE BETWEEN '-1 day' PRECEDING AND CURRENT ROW) FROM r8a",
            "SELECT nth_value(v, 0) OVER (ORDER BY id) FROM r8a",
            "SELECT ntile(0) OVER (ORDER BY id) FROM r8a",
        ])));
    }
    out
}

/// GROUPING SETS / CUBE / ROLLUP with GROUPING(), every output column in the ORDER BY.
fn grouping_sets(rng: &mut Rng) -> Vec<Stmt> {
    let spec = *rng.pick(&[
        "ROLLUP (g, t, m)", "CUBE (g, m)", "GROUPING SETS ((g), (t), ())", "g, ROLLUP (t, m)", "DISTINCT ROLLUP (g, t), CUBE (t)",
        "GROUPING SETS (g, ROLLUP (t, m), CUBE ((g, m)))", "ALL CUBE (g), GROUPING SETS ((), (m))", "GROUPING SETS (())", "ROLLUP ((g, t), m)",
        "CUBE (g, t, m)", "GROUPING SETS ((g, t), (g, t))",
    ]);
    let having = *rng.pick(&["", "", " HAVING count(*) > 1", " HAVING grouping(g) = 1 OR sum(v) > 5", " HAVING g IS NOT NULL"]);
    let wm = rng.chance(0.3);
    let mut out = Vec::new();
    if wm {
        out.push(o("SET work_mem = '64kB'"));
    }
    out.push(q(format!(
        "SELECT g, t, m, grouping(g, t, m) AS gr, count(*) AS n, sum(v) AS s, array_agg(id ORDER BY id) AS ids FROM (SELECT *, v % 3 AS m FROM r8a) a GROUP BY {spec}{having} ORDER BY gr, g, t, m, n, s, ids"
    ), true));
    if rng.chance(0.3) {
        out.push(o(*rng.pick(&[
            "SELECT grouping(v) FROM r8a GROUP BY g",
            "SELECT g, grouping(g) FROM r8a GROUP BY GROUPING SETS ((g), ()) HAVING grouping(g, g) > 0 ORDER BY 1, 2",
            "SELECT g, sum(grouping(g)) FROM r8a GROUP BY g",
            "SELECT g, count(*) OVER (PARTITION BY grouping(g)) FROM r8a GROUP BY ROLLUP (g) ORDER BY 1, 2",
            "SELECT k % 5 AS a, k % 7 AS b, count(*) FROM r8big GROUP BY CUBE (k % 5, k % 7) ORDER BY 1, 2, 3",
        ])));
    }
    if wm {
        out.push(o("RESET work_mem"));
    }
    out
}

/// Recursive CTEs over r8edge (cycles in it) with SEARCH and CYCLE, ordered by every column.
fn search_cycle(rng: &mut Rng) -> Vec<Stmt> {
    let start = rng.range(1, 4);
    let depth = rng.range(2, 6);
    let union = *rng.pick(&["UNION ALL", "UNION ALL", "UNION"]);
    let (search, scol) = *rng.pick(&[
        ("SEARCH DEPTH FIRST BY dst SET ord ", "ord"),
        ("SEARCH BREADTH FIRST BY dst SET ord ", "ord"),
        ("SEARCH DEPTH FIRST BY dst, src SET ord ", "ord"),
        ("", ""),
    ]);
    let (cycle, ccol) = *rng.pick(&[
        ("CYCLE dst SET is_cycle USING path", "is_cycle, path"),
        ("CYCLE dst SET mark TO 'Y' DEFAULT 'N' USING path", "mark, path"),
        ("CYCLE src, dst SET is_cycle USING path", "is_cycle, path"),
        ("", ""),
    ]);
    let extra: Vec<&str> = [scol, ccol].into_iter().filter(|c| !c.is_empty()).collect();
    let sel = extra.iter().map(|c| format!(", {c}")).collect::<String>();
    let order = extra.iter().map(|c| format!("{c}, ")).collect::<String>();
    let mut out = vec![q(format!(
        "WITH RECURSIVE p(src, dst, depth, w) AS (SELECT src, dst, 1, w FROM r8edge WHERE src = {start} {union} SELECT p.src, e.dst, p.depth + 1, p.w + e.w FROM p JOIN r8edge e ON e.src = p.dst WHERE p.depth < {depth}) {search}{cycle} SELECT src, dst, depth, w{sel} FROM p ORDER BY {order}src, dst, depth, w"
    ), true)];
    if rng.chance(0.3) {
        out.push(o(*rng.pick(&[
            "WITH RECURSIVE p(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM p WHERE n < 3) SEARCH DEPTH FIRST BY m SET ord SELECT * FROM p",
            "WITH RECURSIVE p(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM p WHERE n < 3) CYCLE n SET n USING path SELECT * FROM p",
            "WITH RECURSIVE p(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM p WHERE n < 3) SEARCH DEPTH FIRST BY n SET ord CYCLE n SET ord USING path SELECT * FROM p",
            "WITH RECURSIVE p(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM p WHERE n < 3) CYCLE n SET c TO 1 DEFAULT 'x' USING path SELECT * FROM p",
            "WITH RECURSIVE p(n) AS (SELECT 1 INTERSECT SELECT n + 1 FROM p) CYCLE n SET c USING path SELECT * FROM p",
            "WITH RECURSIVE p(n) AS (VALUES (1) UNION ALL SELECT n FROM p) CYCLE n SET c USING path SELECT n, c, path FROM p ORDER BY path",
            "WITH RECURSIVE p(n, a) AS (VALUES (1, '{1}'::int[]) UNION ALL SELECT n + 1, a || n FROM p WHERE n < 4) SEARCH BREADTH FIRST BY n SET o SELECT n, a, o FROM p ORDER BY o DESC",
            "WITH RECURSIVE p(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM p WHERE n < 3) SEARCH DEPTH FIRST BY n SET n SELECT * FROM p",
        ])));
    }
    out
}

/// SQL/JSON and jsonpath: JSON_TABLE, JSON_QUERY / JSON_VALUE / JSON_EXISTS behaviours over
/// constants (one row each: the ON ERROR state of a call site never meets a second row),
/// constructors, IS JSON, jsonpath methods and errors in lax and strict mode.
fn json(rng: &mut Rng) -> Vec<Stmt> {
    const DOCS: &[&str] = &[
        r#"{"a": 1, "b": [1, 2, {"c": "x"}], "d": {"e": null}}"#,
        r#"{"a": "2", "b": [], "s": "2025-03-09T02:30:00", "t": "12:30:45.5+05:30"}"#,
        r#"[1, "two", 3.5, null, true, {"k": [10, 20]}]"#,
        r#"{"items": [{"id": 1, "tags": ["p", "q"]}, {"id": 2, "tags": []}, {"id": 3}]}"#,
        r#""scalar""#,
        r#"{"a": 1e3, "n": -0.0, "big": 12345678901234567890, "f": "1.5e2"}"#,
        r#"{"a": [[1, 2], [3]], "b": {"b": {"b": 1}}}"#,
    ];
    const PATHS: &[&str] = &[
        "$.a", "$.b[*]", "$.b[last]", "$.b[0 to last]", "$[*] ? (@ > 1)", "$.items[*].id", "$.items[*].tags[*]", "$.**",
        "$.**{2}", "$.a.type()", "$.b.size()", "$.*.double()", "$.a + 1", "strict $.missing", "lax $.missing", "$.s.datetime()",
        "$.s.datetime(\"YYYY-MM-DD\\\"T\\\"HH24:MI:SS\")", "$.t.time_tz()", "$.f.decimal(5, 1)", "$.big.bigint()", "$.a.integer()", "$.a.string()",
        "$.a.boolean()", "$.keyvalue()", "$.d.e == null", "$ ? (@.a like_regex \"^[0-9]$\" flag \"i\")", "$.s starts with \"2025\"",
        "exists($.b[*] ? (@ == 2))", "$.a / 0", "strict $.b[5]", "lax $.b[5]", "$[1 to 2]", "$.n.abs()", "$.a.ceiling() - $.a.floor()",
        "$.b[*] ? (@.type() == \"number\") * 2", "$.items[*] ? (exists(@.tags[*] ? (@ == \"q\"))).id", "$.a ? ($x > 0)", "$.big.number()",
        "$.a ? (@ == $\"x\")", "last", "$.\"a\"", "$.a.datetime()", "strict $.*.a",
    ];
    let doc = rng.pick(DOCS).replace('\'', "''");
    let path = rng.pick(PATHS).replace('\'', "''");
    let mut out = Vec::new();
    match rng.below(8) {
        0 => out.push(o(format!("SELECT jsonb_path_query('{doc}', '{path}', '{{\"x\": 1}}'{})", rng.pick(&["", ", true", ", false"])))),
        1 => out.push(o(format!("SELECT jsonb_path_query_array('{doc}', '{path}', '{{\"x\": 2}}'), jsonb_path_exists('{doc}', '{path}', '{{\"x\": 2}}', true), '{doc}'::jsonb @? '{path}', jsonb_path_match('{doc}', 'exists({path})', '{{\"x\": 2}}', true)"))),
        2 => {
            let ret = rng.pick(&["", " RETURNING int", " RETURNING text", " RETURNING numeric(4, 1)", " RETURNING date", " RETURNING boolean", " RETURNING jsonb"]);
            let on = rng.pick(&["", " NULL ON EMPTY", " DEFAULT -1 ON ERROR", " ERROR ON ERROR", " DEFAULT 7 ON EMPTY ERROR ON ERROR", " ERROR ON EMPTY"]);
            out.push(o(format!("SELECT JSON_VALUE(jsonb '{doc}', '{path}' PASSING 1 AS x{ret}{on})")));
        }
        3 => {
            let wrap = rng.pick(&["", " WITH WRAPPER", " WITH CONDITIONAL WRAPPER", " WITHOUT WRAPPER", " WITH UNCONDITIONAL ARRAY WRAPPER"]);
            let quotes = rng.pick(&["", "", " KEEP QUOTES", " OMIT QUOTES"]);
            let ret = rng.pick(&["", " RETURNING text", " RETURNING json", " RETURNING int[]", " RETURNING varchar(3)"]);
            let on = rng.pick(&["", " EMPTY ARRAY ON EMPTY", " EMPTY OBJECT ON ERROR", " ERROR ON ERROR", " NULL ON ERROR", " DEFAULT '\"d\"' ON EMPTY"]);
            out.push(o(format!("SELECT JSON_QUERY(jsonb '{doc}', '{path}' PASSING 1 AS x{ret}{wrap}{quotes}{on})")));
        }
        4 => {
            let on = rng.pick(&["", " TRUE ON ERROR", " ERROR ON ERROR", " UNKNOWN ON ERROR", " FALSE ON ERROR"]);
            out.push(o(format!("SELECT JSON_EXISTS(jsonb '{doc}', '{path}' PASSING 1 AS x{on}), JSON_EXISTS(jsonb '{doc}', 'strict {}'{on})", path.trim_start_matches("strict ").trim_start_matches("lax "))));
        }
        5 => {
            // JSON_TABLE with nested paths, ordinality, EXISTS and FORMAT JSON columns.
            let on = rng.pick(&["", " ERROR ON ERROR", " EMPTY ON ERROR"]);
            let col_on = rng.pick(&["", " DEFAULT 0 ON ERROR", " ERROR ON ERROR", " NULL ON EMPTY"]);
            out.push(q(format!(
                "SELECT jt.* FROM r8j, JSON_TABLE(r8j.j, '$.items[*]' COLUMNS (n FOR ORDINALITY, id int PATH '$.id'{col_on}, has_tags boolean EXISTS PATH '$.tags', tags jsonb PATH '$.tags' FORMAT JSON WITH WRAPPER, NESTED PATH '$.tags[*]' COLUMNS (k FOR ORDINALITY, tag text PATH '$')){on}) jt ORDER BY r8j.id, n, k"
            ), false));
            out.push(q(format!(
                "SELECT * FROM JSON_TABLE(jsonb '{doc}', 'lax $[*]' AS root PASSING 3 AS x COLUMNS (a text PATH '$.a', b int PATH '$.b.size()'{col_on}, NESTED PATH '$.b[*]' AS nb COLUMNS (v text PATH '$' OMIT QUOTES), NESTED PATH '$.*' COLUMNS (t text PATH '$.type()')){on}) ORDER BY 1, 2, 3, 4"
            ), true));
        }
        6 => {
            // Constructors, IS JSON and the aggregates (ordered).
            out.push(o(format!("SELECT JSON('{doc}'{}), JSON_SCALAR({}), JSON_SERIALIZE(JSON('{doc}') RETURNING {})", rng.pick(&["", " WITH UNIQUE KEYS", " WITHOUT UNIQUE"]), rng.pick(&["1.50", "'x'", "now()::date - now()::date", "NULL", "true", "'2025-01-01'::date"]), rng.pick(&["text", "bytea", "varchar(5)"]))));
            out.push(o(format!("SELECT '{doc}' IS JSON, '{doc}' IS JSON OBJECT, '{doc}' IS JSON ARRAY WITH UNIQUE KEYS, '{doc}' IS JSON SCALAR, '{{\"a\":1,\"a\":2}}' IS JSON WITH UNIQUE KEYS, '{doc}'::json IS NOT JSON VALUE")));
            out.push(o(format!("SELECT JSON_OBJECT('a': 1, 'b' VALUE NULL {} {}), JSON_ARRAY(1, NULL, 'x' {}), JSON_OBJECT('k': '{doc}' FORMAT JSON)", rng.pick(&["NULL ON NULL", "ABSENT ON NULL"]), rng.pick(&["", "WITH UNIQUE KEYS", "RETURNING jsonb"]), rng.pick(&["NULL ON NULL", "ABSENT ON NULL", "RETURNING text"]))));
            out.push(o(format!("SELECT JSON_OBJECTAGG(t : v {} {}), JSON_ARRAYAGG(v ORDER BY id {}) FROM r8a WHERE t IS NOT NULL OR {}", rng.pick(&["NULL ON NULL", "ABSENT ON NULL"]), rng.pick(&["", "WITH UNIQUE KEYS"]), rng.pick(&["NULL ON NULL", "ABSENT ON NULL", "RETURNING jsonb"]), rng.chance(0.5))));
        }
        _ => {
            // jsonb functions over the table's documents (each row's own).
            out.push(q(format!("SELECT id, jsonb_path_query_array(j, '{path}', '{{\"x\": 1}}', true), jsonb_typeof(j), j #> '{{b,2}}', j -> 'a', jsonb_strip_nulls(j) = j FROM r8j ORDER BY id"), false));
            out.push(o(format!("SELECT jsonb_set('{doc}', '{{b,0}}', '\"n\"', {}), jsonb_insert('{doc}', '{{b,-1}}', '9', true), '{doc}'::jsonb - 'a', '{doc}'::jsonb #- '{{b,1}}', jsonb_set_lax('{doc}', '{{a}}', NULL, true, '{}')", rng.chance(0.5), rng.pick(&["use_json_null", "delete_key", "return_target", "raise_exception", "bogus"]))));
        }
    }
    out
}

/// Range and multirange operators, functions and aggregates, canonical forms, their errors, and
/// an exclusion constraint.
fn ranges(rng: &mut Rng) -> Vec<Stmt> {
    const LITS: &[&str] = &[
        "int4range(1, 5)", "int4range(1, 5, '[]')", "int4range(5, 5)", "int4range(NULL, 3)", "'empty'::int4range", "int4range(-2, 10, '(]')",
        "int4range(3, 7)", "int4range(7, 9)",
    ];
    const MLITS: &[&str] = &[
        "'{[1,3), [5,8)}'::int4multirange", "'{}'::int4multirange", "int4multirange(int4range(2, 6), int4range(6, 9))", "'{(,0), [10,)}'::int4multirange",
        "'{[1,2], [4,4]}'::int4multirange", "multirange(int4range(3, 7))",
    ];
    let (a, b) = (rng.pick(LITS), rng.pick(LITS));
    let (ma, mb) = (rng.pick(MLITS), rng.pick(MLITS));
    let op = *rng.pick(&["&&", "@>", "<@", "<<", ">>", "&<", "&>", "-|-", "=", "<", ">="]);
    let setop = *rng.pick(&["+", "*", "-"]);
    let mut out = Vec::new();
    match rng.below(7) {
        0 => out.push(o(format!("SELECT {a} {op} {b}, {a} {setop} {b}, range_merge({a}, {b}), lower({a}), upper_inc({b}), isempty({a} * {b}), lower_inf({a})"))),
        1 => out.push(o(format!("SELECT {ma} {op} {mb}, {ma} {setop} {mb}, {ma} {op} {a}, {a} {op} {mb}, range_merge({ma}), lower({ma}), upper({mb}), isempty({ma}), {ma} @> 4"))),
        2 => out.push(q(format!("SELECT id, r {op} {a}, mr {op} {ma}, r {setop} {b}, d @> '2025-01-02'::date, upper(d) - lower(d), n -|- numrange(2.5, 3) FROM r8rng ORDER BY id"), true)),
        3 => {
            out.push(q("SELECT range_agg(r), range_intersect_agg(r), range_agg(mr), range_intersect_agg(mr) FILTER (WHERE mr IS NOT NULL AND mr <> '{}') FROM r8rng".to_string(), false));
            out.push(q("SELECT id, u FROM r8rng, unnest(mr) u ORDER BY id, u".to_string(), true));
            out.push(q(format!("SELECT id FROM r8rng WHERE r {op} {a} ORDER BY id"), true));
        }
        4 => out.push(o(rng.pick(&[
            "SELECT int4range(5, 1)", "SELECT '[3,1]'::int4range", "SELECT int4range(1, 5) - int4range(2, 3)", "SELECT int4range(1, 2) + int4range(5, 6)",
            "SELECT int4range(1, 5, 'x]')", "SELECT '[2147483647,2147483647]'::int4range", "SELECT daterange('2025-01-01', '2025-01-01', '[]')",
            "SELECT numrange('NaN', 1)", "SELECT '(1,1)'::numrange, '[1,1)'::int4range, '(1,2)'::int4range", "SELECT tsrange('2025-01-01', 'infinity', '[]')",
            "SELECT '{[1,2), [2,3)}'::int4multirange, '{[3,4), (,2]}'::int4multirange", "SELECT '{[1,2)'::int4multirange", "SELECT int8range(1, 2) * '[3,4)'::int4range",
            "SELECT range_merge('{}'::int4multirange)", "SELECT lower('empty'::int4range), upper_inf('empty'::int4range)",
        ]).to_string())),
        5 => {
            // An exclusion constraint over ranges: overlaps refused with a DETAIL.
            let k = rng.next() % 100_000;
            out.push(o(format!("CREATE TABLE r8ex{k} (id int, room int, during tsrange, EXCLUDE USING gist (during WITH &&))")));
            out.push(o(format!("INSERT INTO r8ex{k} VALUES (1, 1, '[2025-01-01 10:00, 2025-01-01 11:00)')")));
            out.push(o(format!("INSERT INTO r8ex{k} VALUES (2, 1, '[2025-01-01 {}:00, 2025-01-01 12:00)')", rng.range(9, 11))));
            out.push(o(format!("INSERT INTO r8ex{k} VALUES (3, 2, 'empty'), (4, 2, 'empty')")));
            out.push(q(format!("SELECT id, during FROM r8ex{k} ORDER BY id"), true));
            out.push(o(format!("DROP TABLE r8ex{k}")));
        }
        _ => out.push(o(format!("SELECT {a}::text, {ma}::text, ({a})::int8range IS NULL, '[1.50,2.250)'::numrange, daterange('2025-01-01', '2025-01-10', '(]'), '[\"2025-03-09 02:30\",)'::tstzrange")))
    }
    out
}

/// Full text search: parsing, queries, ranking, headlines, the tsvector / tsquery functions, the
/// table's stored vectors (and its GIN index), and the errors.
fn text_search(rng: &mut Rng) -> Vec<Stmt> {
    let cfg = *rng.pick(&["english", "simple"]);
    let qtext = *rng.pick(&[
        "fox & dog", "quick | slow", "!lazy", "fox <-> den", "jump:*", "rat <2> ran", "(quick | fox) & !dog", "run:A", "dog & !(eat | world)", "cafe",
        "über", "a & the", "brown <-> fox's",
    ]);
    let web = *rng.pick(&["\"quick brown\" -dog", "fox or rat", "-fox", "\"\"", "dog-eat-dog", "rats ran", "or or"]);
    let mut out = Vec::new();
    match rng.below(6) {
        0 => out.push(o(format!("SELECT to_tsvector('{cfg}', '{}'), to_tsquery('{cfg}', '{qtext}'), plainto_tsquery('{cfg}', '{}'), phraseto_tsquery('{cfg}', '{}'), websearch_to_tsquery('{cfg}', '{web}')", rng.pick(WORDS).replace('\'', "''"), rng.pick(WORDS).replace('\'', "''"), rng.pick(WORDS).replace('\'', "''")))),
        1 => {
            let q8 = format!("to_tsquery('english', '{qtext}')");
            if rng.chance(0.4) {
                out.push(o("SET enable_seqscan = off"));
            }
            out.push(q(format!("SELECT id, ts_rank(tsv, {q8}), ts_rank_cd(tsv, {q8}, {}), tsv @@ {q8} FROM r8doc ORDER BY id", rng.pick(&["0", "1", "2", "32", "4|1"])), true));
            out.push(q(format!("SELECT id FROM r8doc WHERE tsv @@ {q8} ORDER BY id"), true));
            out.push(o("RESET enable_seqscan"));
        }
        2 => out.push(q(format!("SELECT id, ts_headline('english', body, to_tsquery('english', '{qtext}'), '{}') FROM r8doc ORDER BY id", rng.pick(&["", "MaxWords=3, MinWords=1", "StartSel=<, StopSel=>, HighlightAll=true", "MaxFragments=2, FragmentDelimiter=..."])), true)),
        3 => out.push(o(format!("SELECT setweight(to_tsvector('{cfg}', 'quick fox'), 'A') || to_tsvector('{cfg}', 'lazy dog'), strip(to_tsvector('{cfg}', 'a fox a fox')), ts_delete(to_tsvector('english', 'fat rats run'), 'rat'), ts_filter(setweight(to_tsvector('fox'), 'B'), '{{a,b}}'), tsvector_to_array('b:2 a:1'::tsvector), array_to_tsvector('{{z,y,z}}'), length(to_tsvector('{cfg}', '{}'))", rng.pick(WORDS).replace('\'', "''")))),
        4 => out.push(o(format!("SELECT numnode(to_tsquery('{cfg}', '{qtext}')), querytree(to_tsquery('{cfg}', '{qtext}')), to_tsquery('{cfg}', '{qtext}') && 'x'::tsquery, !! 'a'::tsquery, 'a'::tsquery <-> 'b <-> c'::tsquery, tsquery_phrase('a', 'b', 3), ts_rewrite('a & b'::tsquery, 'a'::tsquery, 'c | d'::tsquery), 'fat:ab & cat'::tsquery @> 'cat'::tsquery"))),
        _ => out.push(o(rng.pick(&[
            "SELECT to_tsquery('english', 'fox & ')", "SELECT to_tsquery('nosuchcfg', 'fox')", "SELECT ts_headline('english', 'a b', 'a'::tsquery, 'MaxWords=0')",
            "SELECT 'a:1x'::tsvector", "SELECT to_tsquery('simple', '<->')", "SELECT setweight('a'::tsvector, 'E')", "SELECT tsquery_phrase('a', 'b', -1)",
            "SELECT 'a <0> b'::tsquery, 'a <16384> b'::tsquery", "SELECT to_tsvector('english', NULL), ts_rank(NULL, 'a')",
            "SELECT unnest('fat:2,4A cat:3B rat:5C'::tsvector)", "SELECT 'a & !b | c'::tsquery, '!(a & b)'::tsquery",
        ]).to_string())),
    }
    out
}

/// Large objects with oids of their own (a database's oid space: the same on both sides),
/// descriptors in a transaction, and the server-side functions' errors.
fn large_objects(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let lo = 880_000 + k % 100_000;
    let mut out = vec![o(format!("SELECT lo_from_bytea({lo}, '\\x{}')::int8 - {lo}", rng.pick(&["68656c6c6f20776f726c64", "", "00ff00ff", "41424344454647"])))];
    out.push(o(format!("SELECT lo_get({lo}), lo_get({lo}, {}, {})", rng.range(0, 4), rng.range(0, 6))));
    out.push(o("BEGIN"));
    let mode = *rng.pick(&["x'60000'::int", "x'40000'::int", "x'20000'::int", "x'60000'::int"]);
    out.push(o(format!("SELECT lo_open({lo}, {mode})")));
    for s in [
        "SELECT loread(0, 3)".to_string(),
        format!("SELECT lo_lseek(0, {}, {})", rng.range(-2, 6), rng.range(0, 3)),
        "SELECT lowrite(0, '\\x5a5a')".into(),
        "SELECT lo_tell(0), lo_lseek64(0, 0, 2)".into(),
        format!("SELECT lo_truncate(0, {})", rng.range(-1, 8)),
        "SELECT loread(0, 100)".into(),
    ] {
        if rng.chance(0.7) {
            out.push(o(s));
        }
    }
    out.push(o("SELECT lo_close(0)"));
    out.push(o(*rng.pick(&["COMMIT", "ROLLBACK"])));
    out.push(o(format!("SELECT lo_get({lo})")));
    if rng.chance(0.5) {
        out.push(o(format!("SELECT lo_put({lo}, {}, '\\x7171')", rng.range(0, 20))));
        out.push(o(format!("SELECT lo_get({lo}), length(lo_get({lo}))")));
    }
    out.push(o(format!("SELECT oid::int8 - {lo}, lomowner::regrole FROM pg_largeobject_metadata WHERE oid = {lo}")));
    out.push(o(rng.pick(&[
        format!("SELECT lo_from_bytea({lo}, '\\x00')"), format!("SELECT lo_open({}, x'40000'::int)", lo + 1), format!("SELECT lo_unlink({})", lo + 1),
        format!("SELECT lo_create({lo})"), format!("COMMENT ON LARGE OBJECT {lo} IS 'c'"), format!("ALTER LARGE OBJECT {lo} OWNER TO r7_alice"),
        format!("GRANT SELECT ON LARGE OBJECT {lo} TO r7_bob"), "SELECT lo_open(0, x'40000'::int)".into(), format!("SELECT lo_open({lo}, 0)"),
        "SELECT loread(7, 1)".into(),
    ]).clone()));
    out.push(o(format!("SELECT lo_unlink({lo})")));
    out.push(o(format!("SELECT lo_get({lo})")));
    out
}

/// COPY FROM with the newer options (ON_ERROR, LOG_VERBOSITY, REJECT_LIMIT, DEFAULT, HEADER
/// MATCH, FORCE_NULL / FORCE_NOT_NULL, WHERE, FREEZE) over rows some of which fail; COPY TO with
/// quoting options; option errors.
fn copy(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let t = format!("r8c{k}");
    let mut out = vec![o(format!("CREATE TABLE {t} (id int NOT NULL, a text, b int DEFAULT 42, c date)"))];
    let csv = rng.chance(0.5);
    let (sep, null) = if csv { (",", "") } else { ("\t", "\\N") };
    let mut lines = Vec::new();
    let header = csv && rng.chance(0.4);
    if header {
        lines.push(rng.pick(&["id,a,b,c", "id,a,b,c", "id,a,x,c", "id,a,b"]).to_string());
    }
    for i in 1..=rng.range(2, 6) {
        let a = *rng.pick(&["x", "", "\\D", "q\"q", "multi word", null]);
        let a = if csv && a.contains('"') { format!("\"{}\"", a.replace('"', "\"\"")) } else { a.to_string() };
        let b = *rng.pick(&["1", "-7", "zz", "", "\\D", "99999999999", null, "3"]);
        let c = *rng.pick(&["2025-01-31", "2025-13-01", "", "infinity", null, "2025-02-29"]);
        let line = match rng.below(10) {
            0 => format!("{i}{sep}{a}{sep}{b}"),
            1 => format!("{i}{sep}{a}{sep}{b}{sep}{c}{sep}extra"),
            2 => format!("{null}{sep}{a}{sep}{b}{sep}{c}"),
            _ => format!("{i}{sep}{a}{sep}{b}{sep}{c}"),
        };
        lines.push(line);
    }
    let mut opts = vec![if csv { "FORMAT csv" } else { "FORMAT text" }.to_string()];
    if header {
        opts.push(rng.pick(&["HEADER MATCH", "HEADER true", "HEADER"]).to_string());
    }
    for (p, opt) in [
        (0.4, "ON_ERROR ignore"),
        (0.2, "LOG_VERBOSITY verbose"),
        (0.15, "REJECT_LIMIT 2"),
        (0.25, "DEFAULT '\\D'"),
        (0.15, "FORCE_NULL (a)"),
        (0.1, "FORCE_NOT_NULL (a, b)"),
        (0.1, "FORCE_NULL *"),
        (0.05, "ON_ERROR stop"),
        (0.05, "LOG_VERBOSITY silent"),
    ] {
        if rng.chance(p) {
            opts.push(opt.to_string());
        }
    }
    let where_ = if rng.chance(0.2) { " WHERE b IS DISTINCT FROM 1" } else { "" };
    let cols = *rng.pick(&["", "", " (id, a, b, c)", " (id, b, a, c)"]);
    let freeze = rng.chance(0.15);
    if freeze {
        out.push(o("BEGIN"));
        if rng.chance(0.5) {
            out.push(o(format!("TRUNCATE {t}")));
        }
        opts.push("FREEZE".into());
    }
    out.push(dml(format!("COPY {t}{cols} FROM STDIN ({}){where_}\n\\copy-data\n{}\n", opts.join(", "), lines.join("\n")), &t));
    if freeze {
        out.push(o("COMMIT"));
    }
    out.push(q(format!("SELECT id, a, b, c FROM {t} ORDER BY id, a, b, c"), true));
    out.push(o(format!("COPY {t} TO STDOUT ({})", rng.pick(&[
        "FORMAT csv, HEADER, FORCE_QUOTE *", "FORMAT csv, FORCE_QUOTE (a), NULL 'nil', DELIMITER '|'", "FORMAT text, NULL 'NULL'", "FORMAT csv, QUOTE '''', ESCAPE '\\'",
        "FORMAT csv, HEADER MATCH", "FORMAT binary, HEADER", "FORMAT csv, FORCE_NOT_NULL (a)", "FORMAT text, ON_ERROR ignore", "FORMAT csv, DEFAULT 'x'",
    ]))));
    out.push(o(rng.pick(&[
        format!("COPY {t} FROM STDIN (FORMAT binary, ON_ERROR ignore)"), format!("COPY {t} FROM STDIN (DELIMITER ',,')"), format!("COPY {t} FROM STDIN (FORMAT csv, DEFAULT 'x', NULL 'x')"),
        format!("COPY {t} FROM STDIN (REJECT_LIMIT 1)"), format!("COPY {t} FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 0)"), format!("COPY {t} FROM STDIN (FORMAT csv, FORCE_QUOTE (a))"),
        format!("COPY {t} FROM STDIN (FORMAT csv, DELIMITER ',', QUOTE ',')"), format!("COPY {t} FROM STDIN (ON_ERROR whatever)"), format!("COPY {t} (id, id) FROM STDIN"),
        format!("COPY {t} FROM STDIN (FORMAT csv, HEADER MATCH, ON_ERROR ignore)\n\\copy-data\nid,a,b,c\n1,x,y,2025-01-01\n2,z,3,bad\n"),
    ]).clone()));
    out.push(o(format!("DROP TABLE {t}")));
    out
}

/// LISTEN / NOTIFY across sessions 2 and 3 (session 1 only sends: its notifications are the
/// in-process session's): delivery at commit, duplicates folded, rolled back ones dropped, the
/// channel and payload checks.
fn notify(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let ch = format!("r8ch{k}");
    let mut out = Vec::new();
    let mut p = |s: String| out.push(o(s));
    p(format!("\\s2 LISTEN {ch}"));
    if rng.chance(0.5) {
        p(format!("\\s3 LISTEN {ch}"));
    }
    p(format!("NOTIFY {ch}, 'from s1'"));
    p("\\s2 SELECT 1".into());
    p("\\s3 BEGIN".into());
    p(format!("\\s3 NOTIFY {ch}, 'a'"));
    p(format!("\\s3 NOTIFY {ch}, 'a'"));
    p(format!("\\s3 SELECT pg_notify('{ch}', {})", rng.pick(&["'b'", "NULL", "''", "repeat('x', 8000)"])));
    if rng.chance(0.4) {
        p("\\s3 SAVEPOINT s".into());
        p(format!("\\s3 NOTIFY {ch}, 'in savepoint'"));
        p(format!("\\s3 {}", rng.pick(&["ROLLBACK TO s", "RELEASE s"])));
    }
    p(format!("\\s3 {}", rng.pick(&["COMMIT", "COMMIT", "ROLLBACK", "PREPARE TRANSACTION 'r8n'"])));
    p("\\s2 SELECT 2".into());
    p("\\s3 SELECT pg_listening_channels()".into());
    p("\\s2 SELECT pg_listening_channels(), pg_notification_queue_usage() < 0.5".into());
    p(format!("\\s2 {}", rng.pick(&[format!("UNLISTEN {ch}"), "UNLISTEN *".into(), format!("UNLISTEN r8nope{k}")])));
    p(format!("NOTIFY {ch}, 'after unlisten'"));
    p("\\s2 SELECT 3".into());
    p(rng.pick(&["SELECT pg_notify('', 'x')", "SELECT pg_notify(repeat('c', 64), 'x')", "NOTIFY \"\"", "SELECT pg_notify(NULL, 'x')"]).to_string());
    p("\\s3 UNLISTEN *".into());
    p("\\s2 UNLISTEN *".into());
    out
}

/// Prepared transactions (two-phase commit): prepared on session 2, its row lock waited for by
/// session 3, finished from session 1; the gid and state checks. Every gid is finished in the
/// group (and the runner rolls back any left before dropping a seed's database).
fn two_phase(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let gid = format!("r8gx{k}");
    let id = rng.range(1, 6);
    let mut out = Vec::new();
    let mut p = |s: String| out.push(o(s));
    p("\\s2 BEGIN".into());
    p(format!("\\s2 UPDATE r8a SET v = coalesce(v, 0) + 100 WHERE id = {id} RETURNING id, v"));
    p(format!("\\s2 INSERT INTO r8log (tg, info) VALUES ('2pc', '{gid}')"));
    if rng.chance(0.15) {
        p("\\s2 CREATE TEMP TABLE r8tmp (a int)".into());
    }
    p(format!("\\s2 PREPARE TRANSACTION '{gid}'"));
    p("SELECT gid, owner, database = current_database() AS here FROM pg_prepared_xacts ORDER BY gid".into());
    p(format!("\\s2 PREPARE TRANSACTION '{gid}'"));
    if rng.chance(0.6) {
        p(format!("\\s3& UPDATE r8a SET v = -1 WHERE id = {id} RETURNING id, v"));
    }
    p(rng.pick(&[format!("BEGIN; COMMIT PREPARED '{gid}'"), format!("ROLLBACK PREPARED 'r8none{k}'"), format!("\\s2 BEGIN"), "SELECT 1".into()]).clone());
    p("ROLLBACK".into());
    p("\\s2 ROLLBACK".into());
    p(format!("{} PREPARED '{gid}'", rng.pick(&["COMMIT", "ROLLBACK"])));
    p("\\s3.".into());
    p(format!("{} PREPARED '{gid}'", rng.pick(&["COMMIT", "ROLLBACK"])));
    p(format!("SELECT count(*) FROM pg_prepared_xacts WHERE gid = '{gid}'"));
    out.push(q(format!("SELECT id, v FROM r8a WHERE id = {id}"), false));
    out.push(q(format!("SELECT info FROM r8log WHERE tg = '2pc' AND info = '{gid}'"), false));
    out
}

/// Queries over r8big under a small work_mem, so that PostgreSQL's sorts, hash joins, hash
/// aggregates and set operations spill; every one has a total order (or is compared as a set).
fn spill(rng: &mut Rng) -> Vec<Stmt> {
    let mut out = vec![o(format!("SET work_mem = '{}'", rng.pick(&["64kB", "64kB", "100kB", "256kB"])))];
    if rng.chance(0.3) {
        out.push(o(rng.pick(&["SET enable_hashagg = off", "SET enable_sort = off", "SET enable_mergejoin = off", "SET hash_mem_multiplier = 1", "SET enable_hashjoin = off"]).to_string()));
    }
    let lim = rng.range(5, 40);
    let off = rng.range(0, 1500);
    let qs = [
        (format!("SELECT id, s FROM r8big ORDER BY s, id LIMIT {lim} OFFSET {off}"), true),
        (format!("SELECT k, count(*), min(s), max(id), sum(id) FROM r8big GROUP BY k ORDER BY count(*) DESC, k LIMIT {lim}"), true),
        (format!("SELECT a.id, b.id FROM r8big a JOIN r8big b ON a.k = b.k AND a.id < b.id WHERE a.id % 50 = 0 AND b.id % 7 = 0 ORDER BY a.id, b.id LIMIT {lim}"), true),
        ("SELECT DISTINCT k, length(s) FROM r8big ORDER BY k, length(s)".into(), true),
        (format!("SELECT id FROM r8big WHERE id % 3 = 0 EXCEPT SELECT id FROM r8big WHERE id % 5 = 0 ORDER BY id DESC LIMIT {lim}"), true),
        (format!("SELECT k, s FROM r8big UNION SELECT k + 1, s FROM r8big WHERE id < 100 ORDER BY s, k LIMIT {lim}"), true),
        (format!("SELECT id, sum(k) OVER (PARTITION BY k ORDER BY s ROWS 2 PRECEDING), rank() OVER (ORDER BY s DESC) FROM r8big ORDER BY s LIMIT {lim}"), true),
        ("SELECT count(DISTINCT s), count(DISTINCT k), string_agg(DISTINCT left(s, 1), '' ORDER BY left(s, 1)) FROM r8big".into(), true),
        (format!("SELECT k, array_agg(id ORDER BY id DESC) FILTER (WHERE id < 200) FROM r8big GROUP BY k HAVING count(*) > {} ORDER BY k", rng.range(20, 40)), true),
        (format!("SELECT id, k FROM r8big WHERE k IN (SELECT k FROM r8big WHERE id > {off} GROUP BY k HAVING count(*) > 10) ORDER BY id LIMIT {lim}"), true),
        ("SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY s), mode() WITHIN GROUP (ORDER BY k), percentile_cont(0.25) WITHIN GROUP (ORDER BY id) FROM r8big".into(), true),
    ];
    let (sql, ordered) = rng.pick(&qs).clone();
    if rng.chance(0.3) {
        out.push(o(format!("EXPLAIN (COSTS OFF) {sql}")));
    }
    out.push(q(sql, ordered));
    for s in ["work_mem", "enable_hashagg", "enable_sort", "enable_mergejoin", "hash_mem_multiplier", "enable_hashjoin"] {
        out.push(o(format!("RESET {s}")));
    }
    out
}

impl super::Gen<'_> {
    /// Round 8's statements.
    pub(super) fn round8_stmt(&mut self) -> Vec<Stmt> {
        let rng = &mut self.rng;
        let k = rng.next() % 100_000;
        match rng.below(100) {
            0..=13 => plpgsql(rng, k),
            14..=23 => triggers(rng, k),
            24..=27 => view_and_event_triggers(rng, k),
            28..=37 => windows(rng),
            38..=44 => grouping_sets(rng),
            45..=50 => search_cycle(rng),
            51..=62 => json(rng),
            63..=69 => ranges(rng),
            70..=75 => text_search(rng),
            76..=79 => large_objects(rng, k),
            80..=85 => copy(rng, k),
            86..=89 => notify(rng, k),
            90..=93 => two_phase(rng, k),
            _ => spill(rng),
        }
    }
}
