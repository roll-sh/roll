//! Round 9 of the grammar (seeds from `R9_SEEDS` on), on round 8's shared engine with sessions
//! 2 and 3 over the wire: where the optimisers are working.
//!
//! - Prepared statements executed many times with other parameters across the switch from
//!   custom to generic plans (`plan_cache_mode`, `pg_prepared_statements`' plan counts, EXPLAIN
//!   EXECUTE, parameters that fail folding in a custom plan only, invalidation by ANALYZE and
//!   DDL), over SQL PREPARE and the extended protocol, and PL/pgSQL's cached plans.
//! - Batch execution edges over a few thousand rows: NULLs, integer overflow, numeric scale,
//!   NaN and infinities, collations and text comparison, which division by zero comes first
//!   (quals in order, projections below a LIMIT), LIMIT and FETCH ... WITH TIES.
//! - COPY FROM of up to a few thousand rows (past PostgreSQL's 1,000-row insert buffers) into
//!   tables with unique indexes, BEFORE / AFTER row and statement triggers, partitions; failing
//!   late rows; readers in other sessions before and after commit, a unique key waited for.
//! - Index builds of every access method (btree, hash, gin, gist, spgist, brin; unique, partial,
//!   expression, INCLUDE, CONCURRENTLY, partitioned, failing), then index scans and DML.
//! - Range reads: btree bounds, row comparisons, `= ANY`, cross-type bounds, backward cursors,
//!   index-only scans, a range changed by an uncommitted transaction of another session.
//! - Statistics: pg_stats, pg_stats_ext and reltuples after ANALYZE of a fully sampled table
//!   (the sample is the table: deterministic), per-column targets, extended statistics, and the
//!   planner's row estimates read off EXPLAIN.
//!
//! Objects are named `r9*`; a group's own objects carry a serial so a case stands alone.
//! Steps whose order of execution matters (EXECUTE, session steps) are `Other` statements, so
//! a reduced case keeps them in its setup.

use super::round7::{Lit, ext_step, int4, int8, null, text};
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

/// The shared objects: `r9s`, skewed rows for the plan switch and range reads (`g` is 1 in
/// 49 of 50 rows); `r9n`, a few thousand rows of edge values for batch execution; the helper
/// `r9est`, a query's EXPLAIN as node names and row estimates only (no costs).
pub fn schema(rng: &mut Rng) -> Vec<String> {
    let m = rng.range(1500, 3000);
    let n = rng.range(2000, 4000);
    let mut out: Vec<String> = vec![
        "CREATE TABLE r9s (id int PRIMARY KEY, g int NOT NULL, v int, t text, x numeric(12, 4))".into(),
        format!(
            "INSERT INTO r9s SELECT i, CASE WHEN i % 50 = 0 THEN 2 + i % 7 ELSE 1 END, \
             CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 37) % 101 - 50 END, \
             (ARRAY['a', 'ab', 'B', 'b', 'é', 'z', '', 'a b'])[i % 8 + 1] || (i % 17), \
             round(i / 7.0, 4) FROM generate_series(1, {m}) i"
        ),
        "CREATE INDEX r9s_gv ON r9s (g, v)".into(),
        "CREATE INDEX r9s_t ON r9s (t)".into(),
        "CREATE TABLE r9n (id int PRIMARY KEY, a int, b bigint, c smallint, n numeric(20, 6), m numeric, \
         f float8, t text, tc text COLLATE \"C\", ti text COLLATE \"en-x-icu\", z int)"
            .into(),
        format!(
            "INSERT INTO r9n SELECT i, \
             CASE WHEN i % 97 = 0 THEN 2147483647 WHEN i % 89 = 0 THEN -2147483648 WHEN i % 11 = 0 THEN NULL ELSE (i * 7919) % 20001 - 10000 END, \
             CASE WHEN i % 83 = 0 THEN 9223372036854775807 WHEN i % 13 = 0 THEN NULL ELSE i::bigint * 1000003 END, \
             CASE WHEN i % 79 = 0 THEN 32767 WHEN i % 17 = 0 THEN NULL ELSE i % 200 - 100 END, \
             CASE WHEN i % 19 = 0 THEN NULL ELSE round(i * 3.14159 / 7, 6) END, \
             CASE WHEN i % 23 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN 'Infinity' ELSE round(i::numeric / 3, i % 7) END, \
             CASE WHEN i % 29 = 0 THEN NULL WHEN i % 101 = 0 THEN 'NaN' WHEN i % 103 = 0 THEN '-Infinity' ELSE (i % 64) / 4.0 - 8 END, \
             t, t, t, CASE WHEN i % 41 = 0 THEN NULL ELSE i % 7 END \
             FROM generate_series(1, {n}) i, \
             LATERAL (SELECT CASE WHEN i % 43 = 0 THEN NULL ELSE (ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])[i % 12 + 1] || CASE WHEN i % 3 = 0 THEN '' ELSE (i % 5)::text END END) s (t)"
        ),
        "CREATE FUNCTION r9est(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ \
         DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN ' || q LOOP \
         RETURN NEXT regexp_replace(l, '\\(cost=[0-9.]+\\.\\.[0-9.]+ (rows=[0-9]+) width=[0-9]+\\)', '(\\1)'); \
         END LOOP; END $$"
            .into(),
    ];
    if rng.chance(0.3) {
        out.push("CREATE INDEX r9n_a ON r9n (a)".into());
    }
    out.push("ANALYZE r9s".into());
    if rng.chance(0.6) {
        out.push("ANALYZE r9n".into());
    }
    out
}

/// A parameter of a kind: `g` a group of r9s (common or rare), `i` an id, `w` a width, `v` a
/// value of v, `d` a divisor, `t` a text, `a` an int4 array (as text: `{...}`).
fn param(rng: &mut Rng, kind: char) -> Lit {
    match kind {
        'g' => int4(*rng.pick(&[1, 1, 1, 2, 3, 5, 8, 99])),
        'i' => int4(*rng.pick(&[1, 2, 50, 100, 777, 1499, 2999, 0, -5, 2147483647])),
        'w' => int4(*rng.pick(&[0, 1, 5, 99, 1000, -1])),
        'v' => match rng.below(6) {
            0 => null(23),
            1 => int8(3_000_000_000),
            _ => int4(*rng.pick(&[-50, -1, 0, 7, 49])),
        },
        'd' => int4(*rng.pick(&[1, 2, 0, -1, 7])),
        't' => text(rng.pick(&["a", "ab3", "b", "z", "", "é1", "B"])),
        _ => text(rng.pick(&["{1,2,3}", "{}", "{50,100,NULL}", "{2999,1}"])),
    }
}

/// Prepared templates over the shared tables: SQL, parameter kinds (`param`). Results differ
/// between plans in only one way: a custom plan folds its parameters (`10 / $2` fails while
/// planning even when no row qualifies), a generic plan evaluates them per row.
const PREPARED: &[(&str, &str)] = &[
    ("SELECT count(*), sum(v), min(t) FROM r9s WHERE g = $1", "g"),
    ("SELECT id, v FROM r9s WHERE g = $1 AND v > $2 ORDER BY v, id LIMIT 5", "gv"),
    ("SELECT id, x FROM r9s WHERE id BETWEEN $1 AND $1 + $2 ORDER BY id DESC LIMIT 20", "iw"),
    ("SELECT g, count(*), sum(x) FROM r9s WHERE v < $1 GROUP BY g ORDER BY g", "v"),
    ("SELECT count(*) FROM r9s WHERE g = $1 AND v > 10 / $2", "gd"),
    ("SELECT id, t FROM r9s WHERE t > $1 ORDER BY t, id LIMIT 4", "t"),
    ("SELECT id, g FROM r9s WHERE id = ANY ($1::int[]) ORDER BY id", "a"),
    ("UPDATE r9s SET v = v + $2 WHERE id = $1 RETURNING id, g, v", "id"),
    ("SELECT CASE WHEN $1 > 1 THEN 'many' ELSE (SELECT max(t) FROM r9s WHERE g = $1) END", "g"),
    ("SELECT sum(v) FILTER (WHERE g = $1), count(*) FILTER (WHERE v IS NULL) FROM r9s WHERE id <= $2", "gi"),
    ("SELECT n.id, n.a / $1 FROM r9n n JOIN r9s s ON s.id = n.id WHERE s.g = $2 ORDER BY n.id LIMIT 3", "dg"),
    ("SELECT id FROM r9s WHERE g = $1 ORDER BY id DESC LIMIT 2 OFFSET $2", "gw"),
];

fn sql_type(kind: char) -> &'static str {
    match kind {
        't' | 'a' => "text",
        _ => "int",
    }
}

/// Something that invalidates or changes a prepared statement's plans between executions.
fn invalidation(rng: &mut Rng, k: u64, s: u8) -> Vec<String> {
    let at = |sql: String| if s == 1 { sql } else { format!("\\s{s} {sql}") };
    match rng.below(5) {
        0 => vec![at("ANALYZE r9s".into())],
        1 => vec![at(format!("CREATE INDEX r9s_v{k} ON r9s (v)")), at(format!("DROP INDEX r9s_v{k}"))],
        2 => vec![at(format!("SET plan_cache_mode = {}", rng.pick(&["force_generic_plan", "force_custom_plan", "auto"])))],
        3 => vec![at("SET enable_indexscan = off".into()), at("RESET enable_indexscan".into())],
        _ => vec![at("DISCARD PLANS".into())],
    }
}

/// A prepared statement executed past the custom plans, with other parameters each time, its
/// plans shown now and then and its plan counts read at the end; over SQL on session 1 or
/// over the extended protocol on session 2 or 3.
fn prepared(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let (tpl, kinds) = *rng.pick(PREPARED);
    let kinds: Vec<char> = kinds.chars().collect();
    let mut out = Vec::new();
    let s: u8 = if rng.chance(0.55) { 1 } else { 2 + rng.below(2) as u8 };
    let at = |sql: String| if s == 1 { sql } else { format!("\\s{s} {sql}") };
    let mode = *rng.pick(&["", "", "", "force_generic_plan", "force_custom_plan"]);
    if !mode.is_empty() {
        out.push(o(at(format!("SET plan_cache_mode = {mode}"))));
    }
    let name = if s == 1 { format!("r9p{k}") } else { format!("r9x{k}") };
    let lits = |rng: &mut Rng| kinds.iter().map(|&c| param(rng, c)).collect::<Vec<_>>();
    let args = |l: &[Lit]| l.iter().map(|x| x.sql.clone()).collect::<Vec<_>>().join(", ");
    if s == 1 {
        let types = if rng.chance(0.8) {
            format!(" ({})", kinds.iter().map(|&c| sql_type(c)).collect::<Vec<_>>().join(", "))
        } else {
            String::new()
        };
        out.push(o(format!("PREPARE {name}{types} AS {tpl}")));
    } else {
        let l = lits(rng);
        out.push(o(ext_step(rng, s, &name, Some(tpl), &l, true)));
    }
    for i in 0..rng.range(6, 14) {
        let l = lits(rng);
        if s == 1 {
            if rng.chance(0.15) {
                out.push(o(format!("EXPLAIN (COSTS OFF) EXECUTE {name}({})", args(&l))));
            }
            out.push(o(format!("EXECUTE {name}({})", args(&l))));
        } else {
            out.push(o(ext_step(rng, s, &name, None, &l, false)));
        }
        if i > 3 && rng.chance(0.1) {
            out.extend(invalidation(rng, k, s).into_iter().map(o));
        }
    }
    out.push(o(at(format!(
        "SELECT name, generic_plans, custom_plans, from_sql, parameter_types::text, result_types::text \
         FROM pg_prepared_statements WHERE name = '{name}'"
    ))));
    out.push(o(at(format!("DEALLOCATE {name}"))));
    out.push(o(at("RESET plan_cache_mode".into())));
    out
}

/// Cached plans elsewhere: PL/pgSQL's statements in a loop with other values each time, a
/// result type changed under a prepared statement, a SQL function's query.
fn cached_plans(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    match rng.below(3) {
        0 => {
            let expr = *rng.pick(&[
                "(SELECT count(*) FROM r9s WHERE g = i % 4 + 1)",
                "(SELECT sum(v) FROM r9s WHERE id BETWEEN i * 100 AND i * 100 + 50)",
                "(SELECT count(*) FROM r9s WHERE g = i % 9 AND v > 10 / (i % 6))",
                "(SELECT coalesce(max(v), 0) FROM r9s WHERE g = 99 AND v > 100 / (i - 7))",
                "(SELECT count(*) FROM r9n WHERE a > i * 1000) / (i % 3)",
            ]);
            let mode = *rng.pick(&["auto", "auto", "force_generic_plan", "force_custom_plan"]);
            vec![o(format!(
                "DO $$ DECLARE s bigint := 0; BEGIN SET LOCAL plan_cache_mode = {mode}; \
                 FOR i IN 0..{} LOOP s := s + coalesce({expr}, 0); END LOOP; RAISE NOTICE 'r9 %', s; END $$",
                rng.range(6, 14)
            ))]
        }
        1 => {
            let t = format!("r9w{k}");
            let change = *rng.pick(&["ADD COLUMN c int", "ALTER COLUMN b TYPE varchar(3)", "DROP COLUMN b", "ALTER COLUMN a SET DEFAULT 5", "RENAME COLUMN b TO bb"]);
            let sel = *rng.pick(&["*", "a, b", "a"]);
            vec![
                o(format!("CREATE TABLE {t} (a int, b text)")),
                o(format!("INSERT INTO {t} VALUES (1, 'x'), (2, 'yy'), (3, NULL)")),
                o(format!("PREPARE r9q{k} AS SELECT {sel} FROM {t} WHERE a >= $1 ORDER BY a")),
                o(format!("EXECUTE r9q{k}(1)")),
                o(format!("EXECUTE r9q{k}(2)")),
                o(format!("ALTER TABLE {t} {change}")),
                o(format!("EXECUTE r9q{k}(1)")),
                o(format!("DEALLOCATE r9q{k}")),
                o(format!("DROP TABLE {t}")),
            ]
        }
        _ => {
            let f = format!("r9f{k}");
            let lang = *rng.pick(&["sql", "plpgsql"]);
            let body = if lang == "sql" {
                "SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2"
            } else {
                "BEGIN RETURN (SELECT count(*) FROM r9s WHERE g = $1 AND v >= $2); END"
            };
            let calls: Vec<String> =
                (0..rng.range(6, 12)).map(|_| format!("{f}({}, {})", rng.pick(&[1, 2, 99, 1]), rng.pick(&[-50, 0, 30]))).collect();
            vec![
                o(format!("CREATE FUNCTION {f}(int, int) RETURNS bigint LANGUAGE {lang} STABLE AS $f${body}$f$")),
                q(format!("SELECT {}", calls.join(", ")), true),
                q(format!("SELECT g, {f}(g, -100) FROM generate_series(1, 9) g ORDER BY 1"), true),
                o(format!("DROP FUNCTION {f}")),
            ]
        }
    }
}

/// Batch execution's edges over r9n: overflow, division by zero (which row, which qual first,
/// projections under a LIMIT), numeric scale, NaN and infinities, NULLs, collations, ties.
fn batch(rng: &mut Rng) -> Vec<Stmt> {
    let lim = rng.range(1, 40);
    let off = rng.range(0, 1500);
    let m = rng.range(2, 9);
    let r = rng.range(0, m - 1);
    let kk = *rng.pick(&[1, 100, 2147483647, -1]);
    let qs: Vec<(String, bool)> = vec![
        (format!("SELECT id, a + {kk}, c * {} FROM r9n WHERE id % {m} = {r} ORDER BY id", rng.pick(&[2, 400, 1])), true),
        ("SELECT sum(a), sum(b), sum(c), avg(a), avg(c), sum(n), avg(n), sum(m), avg(m), min(m), max(m), count(z), sum(f), avg(f) FROM r9n".into(), true),
        (format!("SELECT z, sum(a * {}), count(*) FROM r9n GROUP BY z ORDER BY z", rng.pick(&[1, 2, 1000, 3])), true),
        (format!("SELECT count(*) FROM r9n WHERE {}", rng.pick(&[
            "a / z > 0", "z <> 0 AND a / z > 0", "a / z > 0 AND z <> 0", "z = 0 OR a / z > 100", "b / z > 0 AND z IS NOT NULL",
            "CASE WHEN z = 0 THEN false ELSE a / z > 0 END", "a % nullif(z, 0) = 1", "n / z > 1", "f / z > 1", "a / (z - z) IS NULL",
        ])), true),
        (format!("SELECT id, a / z, c * 1000 FROM r9n ORDER BY id LIMIT {lim} OFFSET {}", rng.pick(&[0, 0, 5, 6, 40])), true),
        (format!("SELECT id, a / z FROM r9n WHERE id > {off} LIMIT {}", rng.pick(&[1, 5, 6])), false),
        (format!("SELECT id, n * m, n / 7, round(n, 2), trunc(m, 3), m / 3, n + m, scale(m), n % 0.7 FROM r9n WHERE id % {m} = {r} ORDER BY id LIMIT {lim}"), true),
        (format!("SELECT id, n::numeric({}, 2), b::int4, a::int2, f::int, m::int8 FROM r9n WHERE id BETWEEN {off} AND {off} + {lim} ORDER BY id", rng.pick(&[6, 8, 12])), true),
        (format!("SELECT count(*) FILTER (WHERE a IS NULL), count(*) FILTER (WHERE NOT (a > 0)), count(*) FILTER (WHERE a IN (1, NULL)), \
                  bool_and(a > -10001), bool_or(f = 'NaN'), count(DISTINCT f), max(f), min(f), sum(f) FILTER (WHERE f > -9) FROM r9n WHERE id > {off}"), true),
        (format!("SELECT id, greatest(a, z, c), least(n, m), coalesce(a, c, z), nullif(c, z), a IS DISTINCT FROM c FROM r9n WHERE id % {m} = {r} ORDER BY id LIMIT {lim}"), true),
        (format!("SELECT tc, count(*), min(ti), max(t) FROM r9n GROUP BY tc ORDER BY tc LIMIT {lim}"), true),
        (format!("SELECT id, ti FROM r9n ORDER BY ti, id LIMIT {lim} OFFSET {off}"), true),
        (format!("SELECT count(*) FROM r9n WHERE {}", rng.pick(&[
            "tc > 'b'", "ti > 'b'", "t < ti", "tc < ti COLLATE \"C\"", "ti LIKE 'a%'", "tc ILIKE 'A%'", "ti ~ '^[a-z]'", "upper(ti) = ti",
            "ti BETWEEN 'a' AND 'b'", "tc = 'ä' COLLATE \"en-x-icu\"", "t IS DISTINCT FROM tc", "ti IN ('a', 'B', NULL)",
        ])), true),
        (format!("SELECT id, c FROM r9n ORDER BY c NULLS FIRST FETCH FIRST {lim} ROWS WITH TIES"), false),
        (format!("SELECT id, f FROM r9n ORDER BY f DESC NULLS LAST, id LIMIT {lim} OFFSET {}", rng.pick(&[0, 30, 40])), true),
        (format!("SELECT DISTINCT ON (c) c, id, a FROM r9n ORDER BY c, id DESC LIMIT {lim}"), true),
        (format!("SELECT z, count(*) FROM r9n GROUP BY z ORDER BY count(*) DESC, z FETCH FIRST {} ROWS WITH TIES", rng.range(1, 4)), true),
        (format!("SELECT id, f, f = 'NaN', f > 'Infinity', f::numeric, -f FROM r9n WHERE f IS NULL OR f IN ('NaN', '-Infinity', 0, -0.0) ORDER BY id LIMIT {lim}"), true),
        (format!("SELECT a FROM r9n WHERE a NOT IN (SELECT z FROM r9n WHERE id < {}) ORDER BY a LIMIT 5", rng.pick(&[3, 41, 42, 300])), true),
        (format!("SELECT id, a * c, b * c, b + b, c + c, -a, abs(a), a << 1 FROM r9n WHERE id % {m} = {r} ORDER BY id LIMIT {lim}"), true),
    ];
    let (sql, ordered) = rng.pick(&qs).clone();
    let mut out = Vec::new();
    let set = rng.chance(0.3).then(|| *rng.pick(&["enable_indexscan = off", "enable_seqscan = off", "enable_hashagg = off", "enable_sort = off", "work_mem = '64kB'"]));
    if let Some(s) = set {
        out.push(o(format!("SET {s}")));
    }
    if rng.chance(0.2) {
        out.push(o(format!("EXPLAIN (COSTS OFF) {sql}")));
    }
    out.push(q(sql, ordered));
    if let Some(s) = set {
        out.push(o(format!("RESET {}", s.split(' ').next().unwrap())));
    }
    out
}

/// COPY FROM of a few to a few thousand rows (past PostgreSQL's 1,000-row multi-insert
/// buffers) into a table with unique keys, a partitioned one, or one with row and statement
/// triggers (which make PostgreSQL insert row by row); sometimes a late line fails; sometimes
/// session 2 reads before and after the commit, or waits on a key the COPY inserted.
fn copy(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let t = format!("r9c{k}");
    let shape = rng.below(3);
    let mut out = Vec::new();
    let serial = if rng.chance(0.3) { ", s serial" } else { "" };
    match shape {
        0 => {
            out.push(o(format!("CREATE TABLE {t} (id int PRIMARY KEY, a int, b text{serial})")));
            if rng.chance(0.5) {
                out.push(o(format!("CREATE UNIQUE INDEX {t}_b ON {t} (b)")));
            }
        }
        1 => {
            out.push(o(format!("CREATE TABLE {t} (id int, a int, b text{serial}) PARTITION BY RANGE (id)")));
            out.push(o(format!("CREATE TABLE {t}_1 PARTITION OF {t} FOR VALUES FROM (MINVALUE) TO (500)")));
            out.push(o(format!("CREATE TABLE {t}_2 PARTITION OF {t} FOR VALUES FROM (500) TO (1500)")));
            out.push(o(if rng.chance(0.5) {
                format!("CREATE TABLE {t}_3 PARTITION OF {t} DEFAULT")
            } else {
                format!("CREATE TABLE {t}_3 PARTITION OF {t} FOR VALUES FROM (1500) TO (2600)")
            }));
            if rng.chance(0.6) {
                out.push(o(format!("CREATE UNIQUE INDEX {t}_id ON {t} (id)")));
            }
        }
        _ => {
            out.push(o(format!("CREATE TABLE {t} (id int PRIMARY KEY, a int, b text{serial})")));
            out.push(o(format!("CREATE TABLE {t}log (n serial, op text, id int, b text)")));
            let fail = if rng.chance(0.3) { "IF NEW.a = 13 THEN RAISE EXCEPTION 'r9 bad %', NEW.id; END IF;" } else { "" };
            out.push(o(format!(
                "CREATE FUNCTION {t}fb() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN \
                 IF NEW.a < -3 THEN RETURN NULL; END IF; {fail} NEW.b := upper(NEW.b); RETURN NEW; END $f$"
            )));
            out.push(o(format!(
                "CREATE FUNCTION {t}fa() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN \
                 INSERT INTO {t}log (op, id, b) VALUES (TG_WHEN || ' ' || TG_LEVEL, NEW.id, NEW.b); RETURN NULL; END $f$"
            )));
            out.push(o(format!(
                "CREATE FUNCTION {t}fs() RETURNS trigger LANGUAGE plpgsql AS $f$ BEGIN \
                 INSERT INTO {t}log (op, id, b) SELECT 'stmt', count(*), max(b) FROM nt; RETURN NULL; END $f$"
            )));
            if rng.chance(0.6) {
                out.push(o(format!("CREATE TRIGGER {t}b BEFORE INSERT ON {t} FOR EACH ROW EXECUTE FUNCTION {t}fb()")));
            }
            if rng.chance(0.7) {
                out.push(o(format!("CREATE TRIGGER {t}a AFTER INSERT ON {t} FOR EACH ROW EXECUTE FUNCTION {t}fa()")));
            }
            if rng.chance(0.5) {
                out.push(o(format!("CREATE TRIGGER {t}s AFTER INSERT ON {t} REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION {t}fs()")));
            }
        }
    }
    if rng.chance(0.3) {
        out.push(o(format!("INSERT INTO {t} (id, a, b) VALUES (7, 1, 'pre'), (1200, 2, 'pre2')")));
    }
    let n = match rng.below(3) {
        0 => rng.range(3, 12),
        1 => rng.range(900, 1100),
        _ => rng.range(1500, 2599),
    };
    let csv = rng.chance(0.4);
    let sep = if csv { "," } else { "\t" };
    let null = if csv { "" } else { "\\N" };
    // A defect at line p: a duplicate key, a bad integer, a duplicate b, a row no partition takes.
    let p = rng.range(1, n);
    let defect = if rng.chance(0.5) { None } else { Some(rng.below(4)) };
    let mut lines = Vec::with_capacity(n as usize);
    for i in 1..=n {
        let mut id = i.to_string();
        let mut a = ((i * 7) % 50 - 5).to_string();
        let mut b = if i % 17 == 0 { null.to_string() } else { format!("b{i}") };
        if i == p {
            match defect {
                Some(0) => id = (p / 2).max(1).to_string(),
                Some(1) => a = "x1".into(),
                Some(2) => b = "b1".into(),
                Some(_) => id = "5000".into(),
                None => {}
            }
        }
        lines.push(format!("{id}{sep}{a}{sep}{b}"));
    }
    let opts = if csv { " (FORMAT csv)" } else { "" };
    let copy = dml(format!("COPY {t} (id, a, b) FROM STDIN{opts}\n\\copy-data\n{}\n", lines.join("\n")), &t);
    let end = *rng.pick(&["COMMIT", "COMMIT", "ROLLBACK"]);
    let key = rng.range(1, n.min(1100));
    match rng.below(5) {
        0 => {
            out.push(o("BEGIN"));
            out.push(copy);
            out.push(o(format!("\\s2 SELECT count(*), max(id) FROM {t}")));
            out.push(o(end));
            out.push(o(format!("\\s2 SELECT count(*), max(id), sum(a) FROM {t}")));
        }
        1 => {
            out.push(o("\\s2 BEGIN ISOLATION LEVEL REPEATABLE READ"));
            out.push(o(format!("\\s2 SELECT count(*) FROM {t}")));
            out.push(copy);
            out.push(o(format!("\\s2 SELECT count(*), max(id) FROM {t}")));
            out.push(o("\\s2 COMMIT"));
            out.push(o(format!("\\s2 SELECT count(*), max(id) FROM {t}")));
        }
        2 => {
            out.push(o("BEGIN"));
            out.push(copy);
            let tail = *rng.pick(&["", " ON CONFLICT DO NOTHING", " RETURNING id"]);
            out.push(o(format!("\\s2& INSERT INTO {t} (id, a, b) VALUES ({key}, 0, 'w{key}'){tail}")));
            out.push(o(end));
            out.push(o("\\s2."));
        }
        _ => out.push(copy),
    }
    out.push(q(format!("SELECT count(*), count(b), sum(id), min(id), max(id), sum(a), count(DISTINCT b) FROM {t}"), true));
    if !serial.is_empty() {
        out.push(q(format!("SELECT min(s), max(s), count(DISTINCT s), sum(s::bigint * id) FROM {t}"), true));
    }
    match shape {
        1 => out.push(q(format!("SELECT tableoid::regclass, count(*), min(id), max(id) FROM {t} GROUP BY 1 ORDER BY 1"), true)),
        2 => {
            out.push(q(format!("SELECT op, count(*), min(id), max(id), max(b) FROM {t}log GROUP BY op ORDER BY op"), true));
            if n < 20 {
                out.push(q(format!("SELECT n, op, id, b FROM {t}log ORDER BY n"), true));
            }
            out.push(o(format!("DROP TABLE {t}log")));
        }
        _ => {}
    }
    out.push(o(format!("DROP TABLE {t}")));
    if shape == 2 {
        out.push(o(format!("DROP FUNCTION {t}fb(), {t}fa(), {t}fs()")));
    }
    out
}

/// Index builds of every access method over a fresh table (unique ones that fail on duplicate
/// keys, partial, expression, INCLUDE, CONCURRENTLY, in a rolled-back transaction, on a
/// partitioned table), the catalog's view of them, DML after them, then queries the indexes
/// serve with sequential scans off.
fn indexes(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let t = format!("r9i{k}");
    let n = if rng.chance(0.5) { rng.range(40, 300) } else { rng.range(2000, 4000) };
    let part = rng.chance(0.2);
    let mut out = Vec::new();
    let cols = "id int, a int, t text, arr int[], j jsonb, r int4range, p point";
    if part {
        out.push(o(format!("CREATE TABLE {t} ({cols}) PARTITION BY RANGE (id)")));
        out.push(o(format!("CREATE TABLE {t}_1 PARTITION OF {t} FOR VALUES FROM (MINVALUE) TO (1000)")));
        out.push(o(format!("CREATE TABLE {t}_2 PARTITION OF {t} FOR VALUES FROM (1000) TO (MAXVALUE)")));
    } else {
        out.push(o(format!("CREATE TABLE {t} ({cols})")));
    }
    let fill = format!(
        "INSERT INTO {t} SELECT i, CASE WHEN i % 23 = 0 THEN NULL ELSE (i * 31) % 97 END, \
         CASE WHEN i % 29 = 0 THEN NULL ELSE (ARRAY['ab', 'abc', 'b', 'B', 'é', 'a b', 'zz'])[i % 7 + 1] || (i % 13) END, \
         ARRAY[i % 5, i % 7], jsonb_build_object('k', i % 6, 'tags', jsonb_build_array(i % 3, 'x' || (i % 4))), \
         int4range(i % 50, i % 50 + i % 9), point(i % 17, i % 11) FROM generate_series"
    );
    out.push(o(format!("{fill}(1, {n}) i")));
    if rng.chance(0.5) {
        out.push(o(format!("ANALYZE {t}")));
    }
    const DEFS: &[&str] = &[
        "btree (a)", "btree (a DESC NULLS FIRST, id)", "btree (lower(t))", "btree (a) INCLUDE (t)", "btree (t text_pattern_ops)",
        "btree (t COLLATE \"C\")", "UNIQUE btree (id)", "UNIQUE btree (a)", "UNIQUE btree (a, id) NULLS NOT DISTINCT", "btree (a) WHERE a > 50",
        "btree (id) WITH (fillfactor = 50)", "btree (abs(a - 40))", "hash (t)", "hash (a)", "gin (arr)", "gin (j)", "gin (j jsonb_path_ops)",
        "gin (to_tsvector('simple', t))", "gist (r)", "gist (p)", "spgist (r)", "spgist (p)", "spgist (t)", "brin (id)",
        "brin (a) WITH (pages_per_range = 2)", "brin (id int4_minmax_multi_ops)", "UNIQUE btree (t)", "gist (r) WHERE NOT isempty(r)",
    ];
    let rollback = rng.chance(0.1);
    if rollback {
        out.push(o("BEGIN"));
    }
    for i in 0..rng.range(1, 3) {
        let def = *rng.pick(DEFS);
        let (unique, def) = match def.strip_prefix("UNIQUE ") {
            Some(d) => ("UNIQUE ", d),
            None => ("", def),
        };
        let (am, rest) = def.split_once(' ').unwrap();
        let conc = if !rollback && !part && rng.chance(0.2) { "CONCURRENTLY " } else { "" };
        let name = if rng.chance(0.3) { String::new() } else { format!("{t}_x{i} ") };
        out.push(o(format!("CREATE {unique}INDEX {conc}{name}ON {t} USING {am} {rest}")));
    }
    if rng.chance(0.15) {
        out.push(o(format!("ALTER TABLE {t} ADD CONSTRAINT {t}_ex EXCLUDE USING gist ({})", rng.pick(&["r WITH &&", "r WITH =", "p WITH ~="]))));
    }
    if rollback {
        out.push(o("ROLLBACK"));
    }
    out.push(q(format!(
        "SELECT c.relname, am.amname, i.indisunique, i.indisvalid, i.indisready, i.indnkeyatts, i.indnatts, i.indnullsnotdistinct, \
         pg_get_indexdef(i.indexrelid) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_am am ON am.oid = c.relam \
         WHERE c.relname LIKE '{t}%' ORDER BY 1"
    ), true));
    if rng.chance(0.5) {
        out.push(o(format!("{fill}({}, {}) i", n + 1, n + rng.range(5, 200))));
        out.push(o(format!("UPDATE {t} SET a = a + 1, t = t || 'u' WHERE id % 5 = 0")));
        out.push(o(format!("DELETE FROM {t} WHERE id % 11 = 0")));
    }
    if rng.chance(0.2) {
        out.push(o(format!("REINDEX TABLE {t}")));
    }
    out.push(o(format!("SET enable_seqscan = {}", rng.pick(&["off", "off", "on"]))));
    if rng.chance(0.3) {
        out.push(o(rng.pick(&["SET enable_bitmapscan = off", "SET enable_indexscan = off", "SET enable_indexonlyscan = off"]).to_string()));
    }
    const QUALS: &[&str] = &[
        "a = 5", "a BETWEEN 10 AND 20", "a IS NULL", "a > 90", "lower(t) = 'abc3'", "t LIKE 'ab%'", "t = 'b4'", "t > 'zz'", "arr @> '{3}'",
        "arr && '{1,6}'", "j @> '{\"k\": 2}'", "j ? 'tags'", "j @? '$.tags[*] ? (@ == \"x1\")'", "r && '[5,10)'", "r @> 7", "isempty(r)",
        "p <@ box '((0,0),(5,5))'", "to_tsvector('simple', t) @@ 'zz1'", "id BETWEEN 100 AND 300", "abs(a - 40) < 3", "a = ANY ('{1,2,96}')",
    ];
    for _ in 0..rng.range(2, 4) {
        let sql = match rng.below(4) {
            0 => format!("SELECT id, a FROM {t} WHERE a < {} ORDER BY a DESC NULLS FIRST, id LIMIT 10", rng.range(0, 97)),
            1 => format!("SELECT id, p <-> point '(3,3)' FROM {t} ORDER BY p <-> point '(3,3)', id LIMIT {}", rng.range(1, 12)),
            _ => format!("SELECT count(*), sum(id), min(t) FROM {t} WHERE {}", rng.pick(QUALS)),
        };
        if rng.chance(0.4) {
            out.push(o(format!("EXPLAIN (COSTS OFF) {sql}")));
        }
        out.push(q(sql, true));
    }
    for s in ["enable_seqscan", "enable_bitmapscan", "enable_indexscan", "enable_indexonlyscan"] {
        out.push(o(format!("RESET {s}")));
    }
    out.push(o(format!("DROP TABLE {t}")));
    out
}

/// Range reads over r9s: btree bounds of both directions, row comparisons, `= ANY`, NULLs,
/// cross-type and empty bounds, collations; under scan settings; a backward scrolling cursor;
/// a range another session changes and commits or rolls back between two reads.
fn ranges(rng: &mut Rng) -> Vec<Stmt> {
    let a = rng.range(-5, 3000);
    let b = a + *rng.pick(&[0, 1, 10, 99, 500]);
    let lim = rng.range(1, 30);
    let g = *rng.pick(&[1, 1, 2, 4, 8, 99]);
    let v = rng.range(-55, 55);
    let v2 = v + rng.range(0, 40);
    let dir = *rng.pick(&["", "DESC"]);
    let out_of = |rng: &mut Rng| rng.pick(&["2147483646", "-2147483647", "2.5", "3000000000", "-3000000000", "NULL", "'7'"]).to_string();
    let qs: Vec<(String, bool)> = vec![
        (format!("SELECT id, v FROM r9s WHERE id BETWEEN {a} AND {b} ORDER BY id {dir} LIMIT {lim}"), true),
        (format!("SELECT id FROM r9s WHERE id > {a} AND id <= {b}"), false),
        (format!("SELECT count(*), sum(v), min(t), max(x) FROM r9s WHERE id >= {a} AND id < {b}"), true),
        (format!("SELECT g, v, id FROM r9s WHERE (g, v) > ({g}, {v}) ORDER BY g, v, id LIMIT {lim}"), true),
        (format!("SELECT g, v, id FROM r9s WHERE (g, v) >= ({g}, {v}) AND (g, v) < ({g}, {v2}) ORDER BY g DESC, v DESC NULLS FIRST, id DESC LIMIT {lim}"), true),
        (format!("SELECT id, v FROM r9s WHERE g = {g} AND v BETWEEN {v} AND {v2} ORDER BY v DESC NULLS LAST, id LIMIT {lim}"), true),
        (format!("SELECT id FROM r9s WHERE g = {g} AND v IS NULL ORDER BY id LIMIT {lim}"), true),
        (format!("SELECT id, v FROM r9s WHERE g IN (1, 3, 99) AND v < {v} ORDER BY id {dir} LIMIT {lim}"), true),
        (format!("SELECT id FROM r9s WHERE id = ANY ('{{{a}, {b}, 1, NULL, {}}}') ORDER BY id DESC", a + 2), true),
        (format!("SELECT id FROM r9s WHERE id {} {} ORDER BY id LIMIT 3", rng.pick(&[">", "<", ">=", "<=", "="]), out_of(rng)), true),
        (format!("SELECT count(*) FROM r9s WHERE id BETWEEN {b} AND {a}"), true),
        (format!("SELECT id, t FROM r9s WHERE t >= '{}' AND t < 'b' ORDER BY t {dir}, id LIMIT {lim}", rng.pick(&["a", "a b", "ab1", ""])), true),
        (format!("SELECT id, t FROM r9s WHERE t COLLATE \"en-x-icu\" BETWEEN 'a' AND 'b' ORDER BY t COLLATE \"en-x-icu\", id LIMIT {lim}"), true),
        (format!("SELECT max(id), min(id), max(v) FROM r9s WHERE id < {a}"), true),
        (format!("SELECT id, v FROM r9s WHERE g = {g} ORDER BY v NULLS FIRST, id LIMIT {lim} OFFSET {}", rng.range(0, 40)), true),
    ];
    let (sql, ordered) = rng.pick(&qs).clone();
    let mut out = Vec::new();
    if rng.chance(0.15) {
        out.push(o("VACUUM r9s"));
    }
    match rng.below(10) {
        0 => {
            out.push(o("BEGIN"));
            out.push(o(format!("DECLARE r9cur SCROLL CURSOR FOR SELECT id, v FROM r9s WHERE id BETWEEN {a} AND {b} ORDER BY id {dir}")));
            for f in ["FETCH 3", "FETCH BACKWARD 2", "FETCH LAST", "FETCH PRIOR", "FETCH ABSOLUTE 2", "MOVE FORWARD ALL", "FETCH BACKWARD 3", "FETCH RELATIVE -1"] {
                if rng.chance(0.7) {
                    out.push(o(format!("{f} FROM r9cur")));
                }
            }
            out.push(o("CLOSE r9cur"));
            out.push(o("COMMIT"));
            return out;
        }
        1 => {
            let end = *rng.pick(&["COMMIT", "ROLLBACK"]);
            out.push(o("\\s2 BEGIN"));
            out.push(o(format!("\\s2 UPDATE r9s SET v = coalesce(v, 0) + 1000 WHERE id BETWEEN {a} AND {} RETURNING id, v", a + 3)));
            out.push(o(format!("\\s2 INSERT INTO r9s VALUES ({}, 1, 7, 'new', 1.5)", 10_000 + a)));
            out.push(q(sql.clone(), ordered));
            out.push(o(format!("\\s2 {end}")));
            out.push(q(sql, ordered));
            return out;
        }
        _ => {}
    }
    let set = rng.chance(0.4).then(|| *rng.pick(&["enable_seqscan", "enable_indexscan", "enable_bitmapscan", "enable_indexonlyscan"]));
    if let Some(s) = set {
        out.push(o(format!("SET {s} = off")));
    }
    if rng.chance(0.3) {
        out.push(o(format!("EXPLAIN (COSTS OFF) {sql}")));
    }
    out.push(q(sql, ordered));
    if let Some(s) = set {
        out.push(o(format!("RESET {s}")));
    }
    out
}

/// Statistics of a table ANALYZE samples whole (at most 300 x its target rows: the sample is
/// the table, so no randomness): pg_stats, reltuples, per-column targets and n_distinct,
/// extended statistics, and the planner's row estimates (`r9est`) that read them.
fn stats(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let t = format!("r9a{k}");
    let n = rng.range(50, 6000);
    let mut out = vec![
        o(format!("CREATE TABLE {t} (id int, g int, u int, s text, f float8, d date, nul int, b bool, arr int[], tv text)")),
        o(format!(
            "INSERT INTO {t} SELECT i, CASE WHEN i % 10 < 6 THEN 1 WHEN i % 10 < 9 THEN 2 + i % 3 ELSE i % 40 END, (i * 7) % {n}, \
             CASE i % 4 WHEN 0 THEN 'common' ELSE 'v' || (i % 200) END, (i % 1000) / 8.0, date '2024-01-01' + i % 400, \
             CASE WHEN i % 3 = 0 THEN i END, i % 7 = 0, ARRAY[i % 4, i % 9], repeat('x', i % 30) FROM generate_series(1, {n}) i"
        )),
    ];
    // The smallest target whose sample (300 rows per unit) holds every row.
    let min_target = (n + 299) / 300;
    if rng.chance(0.3) {
        let col = *rng.pick(&["g", "u", "s", "f", "arr"]);
        let target = *rng.pick(&[min_target.max(1), min_target.max(10) + 3, 100, 0, -1]);
        out.push(o(format!("ALTER TABLE {t} ALTER COLUMN {col} SET STATISTICS {target}")));
    }
    if rng.chance(0.2) {
        out.push(o(format!("SET default_statistics_target = {}", rng.pick(&[min_target.max(25), 1000, min_target.max(1)]))));
    }
    if rng.chance(0.2) {
        out.push(o(format!("ALTER TABLE {t} ALTER COLUMN {} SET (n_distinct = {})", rng.pick(&["u", "s", "g"]), rng.pick(&["-0.5", "50", "-1", "0"]))));
    }
    let ext = rng.chance(0.4);
    if ext {
        let kinds = *rng.pick(&["", " (ndistinct)", " (dependencies)", " (mcv)", " (ndistinct, dependencies)"]);
        let on = *rng.pick(&["g, (u % 10)", "g, s", "(g + 1), b", "g, u, b"]);
        out.push(o(format!("CREATE STATISTICS {t}_st{kinds} ON {on} FROM {t}")));
    }
    // (An UPDATE moves rows in PostgreSQL's heap: correlation then reads its physical order,
    // which the engine needn't share. Without one, correlation is compared to one decimal:
    // the exact values differ, routed in STATUS.md.)
    let updated = rng.chance(0.3);
    if updated {
        out.push(o(format!("UPDATE {t} SET g = 5, nul = NULL WHERE id % 50 = 0")));
    }
    let corr = if updated { "NULL" } else { "round(correlation::numeric, 1)" };
    out.push(o(rng.pick(&[format!("ANALYZE {t}"), format!("ANALYZE {t}"), format!("ANALYZE {t} (g, s, arr)"), format!("VACUUM ANALYZE {t}"), format!("ANALYZE (SKIP_LOCKED) {t}")]).clone()));
    out.push(o("RESET default_statistics_target"));
    let cols: Vec<&str> = ["id", "g", "u", "s", "f", "d", "nul", "b", "arr", "tv"].into_iter().filter(|_| rng.chance(0.35)).collect();
    let cols = if cols.is_empty() { vec!["g"] } else { cols };
    out.push(q(format!(
        "SELECT attname, inherited, null_frac, avg_width, n_distinct, most_common_vals::text, most_common_freqs::text, \
         histogram_bounds::text, {corr}, most_common_elems::text, most_common_elem_freqs::text, elem_count_histogram::text \
         FROM pg_stats WHERE tablename = '{t}' AND attname IN ('{}') ORDER BY attname, inherited",
        cols.join("', '")
    ), true));
    out.push(q(format!("SELECT reltuples FROM pg_class WHERE relname = '{t}'"), true));
    if ext {
        out.push(q(format!(
            "SELECT statistics_name, attnames::text, exprs::text, kinds::text, inherited, n_distinct::text, dependencies::text, \
             most_common_vals::text, most_common_freqs::text, most_common_base_freqs::text FROM pg_stats_ext WHERE tablename = '{t}' ORDER BY 1"
        ), true));
        out.push(q(format!(
            "SELECT statistics_name, expr, null_frac, n_distinct, most_common_vals::text, histogram_bounds::text, {corr} \
             FROM pg_stats_ext_exprs WHERE tablename = '{t}' ORDER BY 1, 2"
        ), true));
    }
    const QS: &[&str] = &[
        "SELECT * FROM {t} WHERE g = 1", "SELECT * FROM {t} WHERE g = 3", "SELECT * FROM {t} WHERE g = 39", "SELECT * FROM {t} WHERE u = 5",
        "SELECT * FROM {t} WHERE u < 100", "SELECT * FROM {t} WHERE s = 'common'", "SELECT * FROM {t} WHERE s = 'v7'", "SELECT * FROM {t} WHERE s LIKE 'v1%'",
        "SELECT * FROM {t} WHERE f < 10", "SELECT * FROM {t} WHERE f BETWEEN 20 AND 30", "SELECT * FROM {t} WHERE d > '2024-06-01'",
        "SELECT * FROM {t} WHERE nul IS NULL", "SELECT * FROM {t} WHERE nul > 100", "SELECT * FROM {t} WHERE b", "SELECT * FROM {t} WHERE NOT b",
        "SELECT * FROM {t} WHERE arr @> '{2}'", "SELECT * FROM {t} WHERE g = 1 AND u % 10 = 1", "SELECT * FROM {t} WHERE g = 2 AND s = 'common'",
        "SELECT * FROM {t} WHERE g + 1 = 2", "SELECT * FROM {t} WHERE tv = 'xxx'", "SELECT g, count(*) FROM {t} GROUP BY g",
        "SELECT DISTINCT g, u % 10 FROM {t}", "SELECT g, s FROM {t} GROUP BY g, s", "SELECT * FROM {t} a JOIN {t} b ON a.g = b.g WHERE a.u < 10",
        "SELECT * FROM {t} WHERE g IN (1, 2, 3) AND b",
    ];
    for _ in 0..rng.range(1, 3) {
        let sql = rng.pick(QS).replace("{t}", &t);
        // (Not a query: the runner would re-run it under planner settings and expect its rows,
        // a plan, not to change.)
        out.push(o(format!("SELECT * FROM r9est($${sql}$$)")));
        if rng.chance(0.15) {
            out.push(o(format!("EXPLAIN {sql}")));
        }
    }
    out.push(o(format!("DROP TABLE {t}")));
    out
}

impl super::Gen<'_> {
    /// Round 9's statements.
    pub(super) fn round9_stmt(&mut self) -> Vec<Stmt> {
        let rng = &mut self.rng;
        let k = rng.next() % 100_000;
        match rng.below(100) {
            0..=19 => prepared(rng, k),
            20..=26 => cached_plans(rng, k),
            27..=44 => batch(rng),
            45..=56 => copy(rng, k),
            57..=70 => indexes(rng, k),
            71..=84 => ranges(rng),
            _ => stats(rng, k),
        }
    }
}

