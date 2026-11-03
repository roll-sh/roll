//! Round 10 of the grammar (seeds from `R10_SEEDS` on), on round 8's shared engine with
//! sessions 2 and 3 over the wire: where round 10's optimisers work.
//!
//! - Nested loops whose inner side is a parameterized index scan rescanned per outer row: many
//!   outer rows, empty outer sides, NULL outer values, LIMIT above, LATERAL with LIMIT inside,
//!   semi- and anti-joins, correlated sub-selects; joins forced to nested loops, rescans counted
//!   with EXPLAIN ANALYZE (`r10an`).
//! - Custom against generic plans: STABLE casts of text parameters (`$1::int[]`, `$1::date`,
//!   `$1::regclass`), untyped parameters (SQL PREPARE without types and untyped Parse), and a
//!   re-plan after DDL (an index made or dropped, a column's type changed under the statement).
//! - Batch execution: filters over NULL-heavy, collation-sensitive and overflowing data (with
//!   PostgreSQL's qual order by cost), group keys, hash-join probes with duplicate keys, NULL
//!   keys, `-0` / `NaN` float keys and numeric keys of other scales.
//! - Queue tables: `DELETE ... WHERE id = (SELECT min(id) ...)`, SKIP LOCKED, with session 2
//!   inserting and deleting concurrently (a delete waiting on another's, then rechecked).
//! - Statistics views over many tables: pg_stat_user_tables after forced flushes,
//!   pg_stat_xact_user_tables across savepoints, VACUUM / ANALYZE counts, resets, TRUNCATE.
//!
//! Objects are named `r10*`; a group's own objects carry a serial so a case stands alone.
//! Their tables don't autovacuum (see round 9).

use super::round7::{Lit, ext_step, int4, null, text};
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

/// The shared objects: `r10o` (outer rows: `k` dense, `kn` a third NULL), `r10i` (inner rows,
/// indexed on `k`, `(k, v)` and `t`; `k` NULL now and then), `r10b` (batch edges: `a` 70%
/// NULL, `b` near int8's limit, numeric keys of several scales, `-0` and `NaN` floats, text
/// under three collations, group keys), and `r10an`, EXPLAIN ANALYZE without timing or memory.
pub fn schema(rng: &mut Rng) -> Vec<String> {
    let no = rng.range(200, 2500);
    let ni = rng.range(3000, 9000);
    let km = rng.range(40, 400);
    let nb = rng.range(1500, 4000);
    let texts = "(ARRAY['a', 'B', 'b', 'ä', 'A', 'é', 'Z', '', 'ab', 'aB', '10', '9'])";
    let mut out: Vec<String> = vec![
        "CREATE TABLE r10o (id int PRIMARY KEY, k int, kn int, t text, c text COLLATE \"C\", d date) WITH (autovacuum_enabled = off)".into(),
        format!(
            "INSERT INTO r10o SELECT i, i % {km}, CASE WHEN i % 3 = 0 THEN NULL ELSE (i * 7) % {km} END, \
             {texts}[i % 12 + 1] || (i % 5), {texts}[i % 11 + 1], date '2026-01-01' + i % 400 FROM generate_series(1, {no}) i"
        ),
        "CREATE TABLE r10i (id int PRIMARY KEY, k int, v int, t text, n numeric) WITH (autovacuum_enabled = off)".into(),
        format!(
            "INSERT INTO r10i SELECT i, CASE WHEN i % 29 = 0 THEN NULL ELSE (i * 13) % {} END, \
             CASE WHEN i % 11 = 0 THEN NULL ELSE (i * 37) % 1001 - 500 END, {texts}[i % 12 + 1] || (i % 3), \
             round(i / 3.0, i % 4) FROM generate_series(1, {ni}) i",
            km + km / 4
        ),
        "CREATE INDEX r10i_k ON r10i (k)".into(),
        "CREATE INDEX r10i_t ON r10i (t)".into(),
        "CREATE TABLE r10b (id int PRIMARY KEY, a int, b bigint, n numeric, f float8, g int, gn int, \
         t text, tc text COLLATE \"C\", ti text COLLATE \"en-x-icu\") WITH (autovacuum_enabled = off)"
            .into(),
        format!(
            "INSERT INTO r10b SELECT i, \
             CASE WHEN i % 10 < 7 THEN NULL WHEN i % 97 = 3 THEN 2147483647 WHEN i % 89 = 8 THEN -2147483648 ELSE (i * 7919) % 20001 - 10000 END, \
             CASE WHEN i % 5 = 0 THEN NULL WHEN i % 61 = 0 THEN 9223372036854775807 - i ELSE i::bigint * 100000007 END, \
             CASE WHEN i % 4 = 0 THEN NULL WHEN i % 53 = 0 THEN 1e30 WHEN i % 7 = 1 THEN 1.0 WHEN i % 7 = 2 THEN 1.00 ELSE round(i / 7.0, i % 3) END, \
             CASE WHEN i % 6 = 0 THEN NULL WHEN i % 31 = 0 THEN 'NaN' WHEN i % 37 = 0 THEN '-0' WHEN i % 41 = 0 THEN 0 ELSE (i % 40) / 8.0 END, \
             i % 7, CASE WHEN i % 4 = 0 THEN NULL ELSE i % 13 END, s.t, s.t, s.t \
             FROM generate_series(1, {nb}) i, \
             LATERAL (SELECT CASE WHEN i % 9 = 0 THEN NULL ELSE {texts}[i % 12 + 1] || CASE WHEN i % 2 = 0 THEN '' ELSE (i % 3)::text END END) s (t)"
        ),
        "CREATE FUNCTION r10an(q text) RETURNS SETOF text LANGUAGE plpgsql AS $$ \
         DECLARE l text; BEGIN FOR l IN EXECUTE 'EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) ' || q LOOP \
         IF l !~ '(Memory|Storage|Batches|Buckets|Heap Fetches|Hits:)' THEN RETURN NEXT l; END IF; END LOOP; END $$"
            .into(),
    ];
    if rng.chance(0.5) {
        out.push("CREATE INDEX r10i_kv ON r10i (k, v)".into());
    }
    if rng.chance(0.4) {
        out.push("CREATE INDEX r10b_g ON r10b (gn)".into());
    }
    out.push("ANALYZE r10o".into());
    out.push("ANALYZE r10i".into());
    if rng.chance(0.7) {
        out.push("ANALYZE r10b".into());
    }
    out
}

/// Joins whose inner side is rescanned per outer row: parameterized index scans under many
/// outer rows, none, NULL keys, LIMIT, LATERAL; sometimes forced to nested loops (and the
/// inner side to index scans) and sometimes counted with EXPLAIN ANALYZE.
fn nestloop(rng: &mut Rng) -> Vec<Stmt> {
    let n = *rng.pick(&[0, 1, 3, 40, 300, 5000]);
    let lim = rng.range(1, 60);
    let m = rng.range(1, 5);
    let x = *rng.pick(&[-600, -100, 0, 250, 499]);
    let w = rng.range(0, 3);
    let outer = match rng.below(5) {
        0 => format!("o.id < {n}"),
        1 => format!("o.id BETWEEN {} AND {}", n / 2, n / 2 + lim),
        2 => format!("o.id % {} = {}", m + 1, m - 1),
        3 => "o.id < 0".into(),
        _ => format!("o.kn IS NULL AND o.id < {n}"),
    };
    let qs: Vec<String> = vec![
        format!("SELECT o.id, i.id, i.v FROM r10o o JOIN r10i i ON i.k = o.k WHERE {outer} ORDER BY o.id, i.id LIMIT {lim}"),
        format!("SELECT count(*), sum(i.v), count(i.v), min(i.t) FROM r10o o JOIN r10i i ON i.k = o.k WHERE {outer}"),
        format!("SELECT o.id, o.kn, i.id FROM r10o o LEFT JOIN r10i i ON i.k = o.kn AND i.v > {x} WHERE {outer} ORDER BY o.id, i.id LIMIT {lim}"),
        format!(
            "SELECT o.id, l.id, l.v FROM r10o o CROSS JOIN LATERAL (SELECT i.id, i.v FROM r10i i WHERE i.k = o.kn \
             ORDER BY i.v DESC NULLS LAST, i.id LIMIT {m}) l WHERE {outer} ORDER BY o.id, l.v DESC NULLS LAST, l.id LIMIT {lim}"
        ),
        format!(
            "SELECT o.id, l.c, l.s, l.mx FROM r10o o LEFT JOIN LATERAL (SELECT count(*) c, sum(i.v) s, max(i.id) mx FROM r10i i \
             WHERE i.k = o.kn AND i.v > o.id - 500) l ON true WHERE {outer} ORDER BY o.id LIMIT {lim}"
        ),
        format!("SELECT count(*) FROM r10o o WHERE {} EXISTS (SELECT 1 FROM r10i i WHERE i.k = o.kn AND i.v < {x}) AND {outer}", rng.pick(&["", "NOT"])),
        format!(
            "SELECT o.id FROM r10o o WHERE o.kn {} (SELECT i.k FROM r10i i WHERE i.v > {x}) AND {outer} ORDER BY o.id LIMIT {lim}",
            rng.pick(&["IN", "NOT IN", "= ANY", "<> ALL"])
        ),
        format!("SELECT o.id, count(i.id), sum(i.v) FROM r10o o JOIN r10i i ON i.k BETWEEN o.k AND o.k + {w} WHERE {outer} GROUP BY o.id ORDER BY o.id LIMIT {lim}"),
        format!(
            "SELECT o.id, i.id FROM r10o o JOIN r10i i ON i.t = {} WHERE {outer} ORDER BY 1, 2 LIMIT {lim}",
            rng.pick(&["o.t", "o.c COLLATE \"default\"", "o.t || ''", "upper(o.t)"])
        ),
        format!("SELECT o.id, i.id FROM r10o o JOIN r10i i ON i.k = ANY (ARRAY[o.k, o.kn, NULL]) WHERE {outer} ORDER BY 1, 2 LIMIT {lim}"),
        format!(
            "SELECT o.id, (SELECT max(i.v) FROM r10i i WHERE i.k = o.kn), (SELECT i.id FROM r10i i WHERE i.k = o.k ORDER BY i.id LIMIT 1 OFFSET {w}) \
             FROM r10o o WHERE {outer} ORDER BY o.id LIMIT {lim}"
        ),
        format!("SELECT count(*), sum(j.v), max(j.id) FROM r10o o JOIN r10i i ON i.k = o.k JOIN r10i j ON j.id = i.v + 500 WHERE {outer}"),
        format!("SELECT o.id FROM r10o o LEFT JOIN r10i i ON i.k = o.kn AND i.v > {x} WHERE i.id IS NULL AND {outer} ORDER BY o.id LIMIT {lim}"),
        format!("SELECT count(*) FROM (SELECT 1 FROM r10o o JOIN r10i i ON i.k = o.k WHERE {outer} LIMIT {lim}) s"),
        format!("SELECT o.id, i.id, i.v / (o.id % 5) FROM r10o o JOIN r10i i ON i.k = o.k WHERE {outer} ORDER BY o.id, i.id LIMIT {lim}"),
        format!(
            "SELECT o.id, i.id FROM r10o o JOIN LATERAL (SELECT * FROM r10i i WHERE i.k = o.k AND i.v IS NOT NULL ORDER BY i.v, i.id LIMIT 2) i \
             ON i.v > {x} WHERE {outer} ORDER BY o.id DESC, i.id LIMIT {lim}"
        ),
    ];
    let sql = rng.pick(&qs).clone();
    let mut out = Vec::new();
    let mut set: Vec<&str> = Vec::new();
    if rng.chance(0.5) {
        set.extend(["enable_hashjoin", "enable_mergejoin"]);
    }
    if rng.chance(0.25) {
        set.push(*rng.pick(&["enable_memoize", "enable_material", "enable_seqscan", "enable_bitmapscan", "enable_indexonlyscan"]));
    }
    for s in &set {
        out.push(o(format!("SET {s} = off")));
    }
    match rng.below(10) {
        0 => out.push(q(format!("SELECT * FROM r10an($q${sql}$q$)"), true)),
        1 => out.push(o(format!("EXPLAIN (COSTS OFF) {sql}"))),
        _ => {}
    }
    out.push(q(sql, true));
    for s in &set {
        out.push(o(format!("RESET {s}")));
    }
    out
}

/// A parameter of a kind: `a` an int4 array as text, `i` an id bound, `k` a key (NULL now and
/// then), `d` a date as text, `r` a relation name, `s` an integer as text, `t` a text, `A` a
/// text array.
fn param(rng: &mut Rng, kind: char) -> Lit {
    match kind {
        'a' => match rng.below(8) {
            0 => null(25),
            1 => text("{1,x}"),
            _ => text(rng.pick(&["{1,2,3}", "{}", "{50,100,NULL}", "{2999,1}", "{7}", "{3,3,9}"])),
        },
        'i' => int4(*rng.pick(&[1, 10, 100, 2500, 0])),
        'k' => match rng.below(7) {
            0 => null(23),
            _ => int4(*rng.pick(&[0, 1, 7, 39, 1000, 2])),
        },
        'd' => text(rng.pick(&["2026-01-05", "2026-12-31", "2025-01-01", "infinity", "2026-02-03", "2026-02-30"])),
        'r' => text(rng.pick(&["r10o", "r10i", "pg_class", "r10nope", "public.r10b", "r10o"])),
        's' => text(rng.pick(&["5", " 7", "12", "x", "", "0"])),
        't' => text(rng.pick(&["a", "B1", "", "ä0"])),
        _ => text(rng.pick(&["{a1,B2}", "{}", "{NULL,ä0}", "{\"\",b4}"])),
    }
}

/// Templates: SQL, parameter kinds, whether SQL PREPARE gives the parameters' types. Text
/// parameters cast by STABLE input functions (`$1::int[]`, `$1::date`, `$1::regclass`) are
/// folded by neither plan; untyped parameters take their types from the query.
const PREPARED: &[(&str, &str, bool)] = &[
    ("SELECT count(*), sum(i.v) FROM r10o o JOIN r10i i ON i.k = o.k WHERE o.id = ANY ($1::int[])", "a", true),
    ("SELECT id, k FROM r10o WHERE k = ANY ($1::int[]) AND id < $2 ORDER BY id LIMIT 5", "ai", true),
    ("SELECT count(*), min(id) FROM r10o WHERE d >= $1::date AND d < $1::date + 7", "d", true),
    ("SELECT count(*) FROM r10o WHERE d > $1::timestamptz", "d", true),
    ("SELECT relname FROM pg_class WHERE oid = $1::regclass", "r", true),
    ("SELECT id FROM r10i WHERE k = $1::int ORDER BY id LIMIT 3", "s", true),
    ("SELECT o.id, i.id FROM r10o o JOIN r10i i ON i.k = o.k WHERE o.id = ANY ($1::int[]) ORDER BY 1, 2 LIMIT 20", "a", true),
    ("SELECT id FROM r10o WHERE k = $1 OR $1 IS NULL ORDER BY id LIMIT 3", "k", false),
    ("SELECT $1 || t FROM r10o WHERE id = $2", "ti", false),
    ("SELECT id FROM r10o WHERE k BETWEEN $1 AND $2 + 1 ORDER BY id LIMIT 4", "kk", false),
    ("SELECT coalesce($1, k), count(*) FROM r10o WHERE id < 50 GROUP BY 1 ORDER BY 1", "k", false),
    ("SELECT count(*) FROM r10o WHERE t = ANY ($1)", "A", false),
    ("SELECT $1 + $2", "kk", false),
    ("SELECT id FROM r10o WHERE id IN ($1, $2) ORDER BY id", "kk", false),
    ("SELECT count(*) FROM r10i WHERE v > $1 AND k = $2", "kk", false),
];

fn sql_type(kind: char) -> &'static str {
    match kind {
        'i' | 'k' => "int",
        'A' => "text[]",
        _ => "text",
    }
}

/// DDL between executions that re-plans the statement: an index made, ANALYZE, a column added
/// and dropped, a table of the same name earlier on the search_path, extended statistics.
/// Returns the steps and the steps that undo them after the last execution.
fn replan(rng: &mut Rng, k: u64, at: &dyn Fn(String) -> String) -> (Vec<String>, Vec<String>) {
    match rng.below(5) {
        0 => (vec![at(format!("CREATE INDEX r10o_x{k} ON r10o ({})", rng.pick(&["k", "d", "t", "kn, k"])))], vec![at(format!("DROP INDEX r10o_x{k}"))]),
        1 => (vec![at("ANALYZE r10o".into())], vec![]),
        2 => (vec![at(format!("ALTER TABLE r10o ADD COLUMN z{k} int")), at(format!("ALTER TABLE r10o DROP COLUMN z{k}"))], vec![]),
        3 => (
            vec![at(format!("CREATE SCHEMA r10n{k}")), at(format!("CREATE TABLE r10n{k}.r10o (LIKE public.r10o)")), at(format!("SET search_path = r10n{k}, public"))],
            vec![at("RESET search_path".into()), at(format!("DROP SCHEMA r10n{k} CASCADE"))],
        ),
        _ => (vec![at(format!("CREATE STATISTICS r10st{k} ON k, kn FROM r10o")), at("ANALYZE r10o".into())], vec![at(format!("DROP STATISTICS r10st{k}"))]),
    }
}

/// A prepared statement executed past the custom plans with other parameters, re-planned by
/// DDL partway, its plans shown now and then and its plan counts read at the end; over SQL on
/// session 1 or the extended protocol on session 2 or 3 (typed or untyped Parse).
fn plans(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let (tpl, kinds, typed) = *rng.pick(PREPARED);
    let kinds: Vec<char> = kinds.chars().collect();
    let s: u8 = if rng.chance(0.55) { 1 } else { 2 + rng.below(2) as u8 };
    let at = move |sql: String| if s == 1 { sql } else { format!("\\s{s} {sql}") };
    let mut out = Vec::new();
    let mode = *rng.pick(&["", "", "", "force_generic_plan", "force_custom_plan"]);
    if !mode.is_empty() {
        out.push(o(at(format!("SET plan_cache_mode = {mode}"))));
    }
    let name = if s == 1 { format!("r10p{k}") } else { format!("r10x{k}") };
    let lits = |rng: &mut Rng| kinds.iter().map(|&c| param(rng, c)).collect::<Vec<_>>();
    let args = |l: &[Lit]| l.iter().map(|x| x.sql.clone()).collect::<Vec<_>>().join(", ");
    if s == 1 {
        let types = if typed && rng.chance(0.85) {
            format!(" ({})", kinds.iter().map(|&c| sql_type(c)).collect::<Vec<_>>().join(", "))
        } else {
            String::new()
        };
        out.push(o(format!("PREPARE {name}{types} AS {tpl}")));
    } else {
        let l = lits(rng);
        out.push(o(ext_step(rng, s, &name, Some(tpl), &l, true)));
    }
    let n = rng.range(6, 12);
    let ddl_at = rng.range(3, n);
    let mut undo = Vec::new();
    for i in 0..n {
        if i == ddl_at {
            let (steps, u) = replan(rng, k, &at);
            out.extend(steps.into_iter().map(o));
            undo = u;
        }
        let l = lits(rng);
        if s == 1 {
            if rng.chance(0.2) {
                out.push(o(format!("EXPLAIN (COSTS OFF) EXECUTE {name}({})", args(&l))));
            }
            out.push(o(format!("EXECUTE {name}({})", args(&l))));
        } else {
            out.push(o(ext_step(rng, s, &name, None, &l, false)));
        }
    }
    out.push(o(at(format!(
        "SELECT name, generic_plans, custom_plans, parameter_types::text, result_types::text FROM pg_prepared_statements WHERE name = '{name}'"
    ))));
    out.push(o(at(format!("DEALLOCATE {name}"))));
    out.extend(undo.into_iter().map(o));
    out.push(o(at("RESET plan_cache_mode".into())));
    out
}

/// Filters over r10b's NULL-heavy, overflowing and collation-sensitive columns. PostgreSQL
/// orders a scan's quals by cost (`a + 1 > 0 AND a < 0` tests `a < 0` first), so which
/// overflow or division by zero is raised depends on that order.
const PREDS: &[&str] = &[
    "a > 0", "a IS NULL", "NOT (a > 0)", "a > 0 AND a + 1 > 0", "a + 1 > 0 AND a < 0", "a < 0 AND a + 1 > 0",
    "a * 2 > 0 OR a IS NULL", "b * 2 > 0", "b + b > 0 AND b < 1000000000000", "n::int4 > 0", "n < 100 AND n::int4 > 0",
    "n::int4 > 0 AND n < 100", "f > 1", "f = 'NaN'", "f = 0", "f::numeric > 1", "-a > 0", "tc > 'b'", "ti > 'b'", "t > 'b'",
    "ti < tc COLLATE \"en-x-icu\"", "tc ~ '^[a-z]'", "ti ILIKE 'a%'", "t IN ('a', 'B1', NULL)", "t NOT IN ('a', NULL)",
    "lower(ti) = ti", "gn IS DISTINCT FROM g", "a BETWEEN -100 AND 100", "coalesce(a, gn) > 5", "a / gn > 1",
    "gn <> 0 AND a / gn > 1", "a / gn > 1 AND gn <> 0", "a % 3 = 0 OR b % 3 = 0", "tc = t", "ti = 'b' AND tc < 'c'",
];

/// Batch execution over r10b: filters, group keys (NULLs, collations, `-0` / `0` and numeric
/// scales in one group), overflowing aggregates; hash joins whose probes meet duplicate keys,
/// NULL keys, float and numeric keys equal under other representations, and join filters
/// that overflow; spilling under a small work_mem.
fn batch(rng: &mut Rng) -> Vec<Stmt> {
    let p = *rng.pick(PREDS);
    let lim = rng.range(1, 40);
    let n = rng.range(5, 300);
    let n2 = rng.range(5, 1500);
    let x = *rng.pick(&[-1000, 0, 5000]);
    let big = *rng.pick(&[1, 1000, 100000, 2]);
    let key = *rng.pick(&["g", "gn", "f", "n", "ti", "t", "a"]);
    let qs: Vec<String> = vec![
        format!("SELECT count(*), count(a), sum(a), min(b), max(n), max(f), min(ti) FROM r10b WHERE {p}"),
        format!("SELECT id, a, b, f FROM r10b WHERE {p} ORDER BY id LIMIT {lim}"),
        format!("SELECT gn, ti, count(*), count(a), sum(a) FROM r10b WHERE {p} GROUP BY gn, ti ORDER BY gn NULLS FIRST, ti NULLS FIRST LIMIT {lim}"),
        "SELECT tc, count(*), min(ti), max(t) FROM r10b GROUP BY tc ORDER BY tc NULLS FIRST".into(),
        format!("SELECT count(*), sum(id), min(id) FROM r10b GROUP BY {} ORDER BY 2", rng.pick(&["f", "n", "ti", "gn, f", "n, g"])),
        format!(
            "SELECT g, gn, GROUPING(g, gn), count(*), sum(b) FROM r10b WHERE {p} GROUP BY ROLLUP (g, gn) \
             ORDER BY g NULLS FIRST, gn NULLS FIRST, GROUPING(g, gn) LIMIT {lim}"
        ),
        format!("SELECT sum(a * {big}), sum(b), avg(b), sum(n), avg(f) FROM r10b WHERE {p}"),
        format!("SELECT sum(b * {big}) FROM r10b WHERE {p}"),
        format!("SELECT count(DISTINCT gn), count(DISTINCT ti), array_agg(DISTINCT gn ORDER BY gn) FROM r10b WHERE {p}"),
        format!("SELECT gn, count(*) FROM r10b GROUP BY gn HAVING sum(a) > {x} ORDER BY gn NULLS FIRST"),
        format!("SELECT ti COLLATE \"C\", count(*) FROM r10b GROUP BY 1 ORDER BY 1 NULLS FIRST LIMIT {lim}"),
        format!("SELECT count(*), sum(x.id), sum(y.id) FROM r10b x JOIN r10b y ON x.{key} = y.{key} WHERE x.id < {n}"),
        format!("SELECT count(*), count(y.id), sum(y.id) FROM r10b x LEFT JOIN r10b y ON x.{key} = y.{key} AND y.id < {n2} WHERE x.id < {n}"),
        format!(
            "SELECT count(*), count(x.id), count(y.id) FROM (SELECT * FROM r10b WHERE id < {n}) x {} JOIN (SELECT * FROM r10b WHERE id > {n2}) y ON x.{key} = y.{key}",
            rng.pick(&["FULL", "RIGHT", "LEFT"])
        ),
        format!("SELECT count(*) FROM r10b x WHERE x.{key} {} (SELECT y.{key} FROM r10b y WHERE y.id < {n})", rng.pick(&["IN", "NOT IN"])),
        format!("SELECT count(*) FROM r10b x WHERE {}EXISTS (SELECT 1 FROM r10b y WHERE y.{key} = x.{key} AND y.id < {n})", rng.pick(&["", "NOT "])),
        format!("SELECT x.id, y.id FROM r10b x JOIN r10b y ON x.g = y.g AND x.a < y.a WHERE x.id < {n} AND y.id < {n2} ORDER BY 1, 2 LIMIT {lim}"),
        format!("SELECT count(*) FROM r10b x JOIN r10b y ON x.g = y.g AND x.a + y.a > 0 WHERE x.id < {n} AND y.id < {n2}"),
        format!("SELECT count(*) FROM r10b x JOIN r10b y ON x.tc = y.t WHERE x.id < {n}"),
        format!("SELECT x.gn, count(*), sum(y.b) FROM r10b x JOIN r10b y ON x.gn = y.g WHERE x.id < {n2} GROUP BY x.gn ORDER BY 1"),
    ];
    let sql = rng.pick(&qs).clone();
    let mut out = Vec::new();
    let mut set: Vec<&str> = Vec::new();
    if rng.chance(0.4) {
        set.push(*rng.pick(&["enable_mergejoin = off", "enable_nestloop = off", "enable_hashjoin = off", "enable_hashagg = off", "enable_sort = off"]));
    }
    if rng.chance(0.2) {
        set.push("work_mem = '64kB'");
    }
    for s in &set {
        out.push(o(format!("SET {s}")));
    }
    if rng.chance(0.15) {
        out.push(o(format!("EXPLAIN (COSTS OFF) {sql}")));
    }
    out.push(q(sql, true));
    for s in &set {
        out.push(o(format!("RESET {}", s.split(' ').next().unwrap())));
    }
    out
}

/// A queue table drained by `DELETE ... WHERE id = (SELECT min(id) ...)` and its SKIP LOCKED,
/// UPDATE and CTE forms while session 2 inserts in an open transaction, deletes the same head
/// (waiting, then rechecking a row the other deleted or updated), or skips locked rows.
fn queue(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let t = format!("r10q{k}");
    let deq = [
        format!("DELETE FROM {t} WHERE id = (SELECT min(id) FROM {t}) RETURNING id, p"),
        format!("DELETE FROM {t} WHERE id = (SELECT id FROM {t} ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id, p"),
        format!("UPDATE {t} SET tries = tries + 1 WHERE id = (SELECT min(id) FROM {t} WHERE tries = 0) RETURNING id, tries"),
        format!("WITH x AS (SELECT id FROM {t} ORDER BY id LIMIT 3 FOR UPDATE SKIP LOCKED) DELETE FROM {t} USING x WHERE {t}.id = x.id RETURNING {t}.id"),
        format!("DELETE FROM {t} WHERE id IN (SELECT id FROM {t} ORDER BY id LIMIT 2) RETURNING id"),
        format!("DELETE FROM {t} WHERE id = (SELECT min(id) FROM {t} WHERE p LIKE 'j%') RETURNING id"),
    ];
    let mut out = vec![
        o(format!("CREATE TABLE {t} (id bigserial PRIMARY KEY, p text, tries int NOT NULL DEFAULT 0) WITH (autovacuum_enabled = off)")),
        o(format!("INSERT INTO {t} (p) SELECT 'j' || i FROM generate_series(1, {}) i", rng.range(0, 30))),
    ];
    let d = |rng: &mut Rng| deq[rng.below(deq.len())].clone();
    let end = *rng.pick(&["COMMIT", "COMMIT", "ROLLBACK"]);
    match rng.below(5) {
        0 => {
            for _ in 0..rng.range(3, 8) {
                if rng.chance(0.3) {
                    out.push(dml(format!("INSERT INTO {t} (p) VALUES ('n'), ('m') RETURNING id"), &t));
                }
                out.push(dml(d(rng), &t));
            }
        }
        1 => {
            out.push(o("\\s2 BEGIN"));
            out.push(o(format!("\\s2 INSERT INTO {t} (p) VALUES ('s2a'), ('s2b') RETURNING id")));
            out.push(dml(d(rng), &t));
            out.push(o(format!("\\s2 {end}")));
            out.push(dml(d(rng), &t));
            out.push(dml(d(rng), &t));
        }
        2 => {
            let i = rng.below(3);
            out.push(o("BEGIN"));
            out.push(dml(deq[[0, 2, 5][i]].clone(), &t));
            out.push(o(format!("\\s2& {}", deq[[0, 2, 0][rng.below(3)]])));
            out.push(o(end));
            out.push(o("\\s2."));
        }
        3 => {
            out.push(o("BEGIN"));
            out.push(dml(deq[1].clone(), &t));
            out.push(o(format!("\\s2 {}", deq[*rng.pick(&[1, 3])])));
            out.push(o(format!("\\s2 SELECT count(*), min(id) FROM {t}")));
            out.push(o(end));
        }
        _ => {
            out.push(o("\\s2 BEGIN"));
            out.push(o(format!("\\s2 {}", deq[0])));
            out.push(dml(format!("INSERT INTO {t} (p) VALUES ('x') RETURNING id"), &t));
            out.push(q(format!("SELECT min(id), count(*) FROM {t}"), true));
            out.push(o(format!("\\s2 {end}")));
            out.push(dml(d(rng), &t));
        }
    }
    out.push(q(format!("SELECT count(*), min(id), max(id), sum(tries), string_agg(p, ',' ORDER BY id) FROM {t}"), true));
    out.push(q(format!("SELECT last_value, is_called FROM {t}_id_seq"), true));
    out.push(o(format!("DROP TABLE {t}")));
    out
}

/// The statistics views over many tables: counters after forced flushes (from this session and
/// another), the transaction's own counters across a rolled-back savepoint, a snapshot cached
/// for the transaction, VACUUM / ANALYZE counts, a reset, TRUNCATE.
fn stats(rng: &mut Rng, k: u64) -> Vec<Stmt> {
    let n = rng.range(15, 60);
    let like = format!("relname LIKE 'r10v{k}\\_%'");
    let cols = "relname, n_tup_ins, n_tup_upd, n_tup_del, n_live_tup, n_dead_tup, n_mod_since_analyze, n_ins_since_vacuum";
    let flush = || o("SELECT pg_stat_force_next_flush()");
    let view = |w: &str| q(format!("SELECT {cols} FROM pg_stat_user_tables WHERE {w} ORDER BY relname"), true);
    let mut out = vec![
        o(format!(
            "DO $$ BEGIN FOR j IN 1..{n} LOOP \
             EXECUTE format('CREATE TABLE r10v{k}_%s (id int PRIMARY KEY, v int) WITH (autovacuum_enabled = off)', j); \
             EXECUTE format('INSERT INTO r10v{k}_%s SELECT i, i %% 7 FROM generate_series(1, %s) i', j, j * 3); END LOOP; END $$"
        )),
        o(format!(
            "DO $$ BEGIN FOR j IN 1..{n} LOOP \
             IF j % 3 = 0 THEN EXECUTE format('UPDATE r10v{k}_%s SET v = v + 1 WHERE id %% 4 = 0', j); END IF; \
             IF j % 4 = 1 THEN EXECUTE format('DELETE FROM r10v{k}_%s WHERE id > %s', j, j); END IF; END LOOP; END $$"
        )),
        flush(),
    ];
    let one = |j: i64| format!("relname IN ('r10v{k}_{j}', 'r10v{k}_{}', 'r10v{k}_{}')", j + 1, j + 2);
    for _ in 0..rng.range(1, 4) {
        match rng.below(8) {
            0 => out.push(view(&like)),
            1 => out.push(o(format!("\\s2 SELECT {cols} FROM pg_stat_user_tables WHERE {like} ORDER BY relname"))),
            2 => out.push(q(format!("SELECT count(*), sum(n_tup_ins), sum(n_live_tup), sum(n_dead_tup), count(*) FILTER (WHERE n_live_tup > 10) FROM pg_stat_all_tables WHERE {like}"), true)),
            3 => {
                out.push(o("BEGIN"));
                out.push(o(format!("INSERT INTO r10v{k}_1 VALUES (100000, 1)")));
                out.push(o("SAVEPOINT s"));
                out.push(o(format!("INSERT INTO r10v{k}_2 SELECT i + 100000, 0 FROM generate_series(1, 5) i")));
                out.push(o(format!("DELETE FROM r10v{k}_1 WHERE id < 3")));
                out.push(o("ROLLBACK TO s"));
                out.push(o(format!("UPDATE r10v{k}_3 SET v = 0")));
                out.push(q(format!("SELECT relname, n_tup_ins, n_tup_upd, n_tup_del FROM pg_stat_xact_user_tables WHERE {} ORDER BY relname", one(1)), true));
                out.push(o(*rng.pick(&["COMMIT", "ROLLBACK"])));
                out.push(flush());
                out.push(view(&one(1)));
            }
            4 => {
                let j = rng.range(3, 10);
                out.push(o(format!("VACUUM r10v{k}_{j}")));
                out.push(o(format!("ANALYZE r10v{k}_{}", j + 1)));
                out.push(o(format!("VACUUM ANALYZE r10v{k}_{}", j + 2)));
                out.push(q(format!(
                    "SELECT relname, vacuum_count, analyze_count, last_vacuum IS NOT NULL, last_analyze IS NOT NULL, n_live_tup, n_dead_tup, \
                     n_mod_since_analyze, n_ins_since_vacuum FROM pg_stat_user_tables WHERE {} ORDER BY relname", one(j)
                ), true));
            }
            5 => {
                out.push(o(format!("SELECT pg_stat_reset_single_table_counters('r10v{k}_2'::regclass)")));
                out.push(o(format!("TRUNCATE r10v{k}_6")));
                out.push(flush());
                out.push(view(&format!("relname IN ('r10v{k}_2', 'r10v{k}_6')")));
            }
            6 => {
                out.push(o("BEGIN"));
                out.push(q(format!("SELECT n_tup_ins, n_live_tup FROM pg_stat_user_tables WHERE relname = 'r10v{k}_1'"), true));
                out.push(o(format!("\\s2 INSERT INTO r10v{k}_1 VALUES (200000, 0)")));
                out.push(o("\\s2 SELECT pg_stat_force_next_flush()"));
                out.push(q(format!("SELECT n_tup_ins, n_live_tup FROM pg_stat_user_tables WHERE relname = 'r10v{k}_1'"), true));
                out.push(o("COMMIT"));
                out.push(q(format!("SELECT n_tup_ins, n_live_tup FROM pg_stat_user_tables WHERE relname = 'r10v{k}_1'"), true));
            }
            _ => out.push(q(format!(
                "SELECT pg_stat_get_live_tuples('r10v{k}_1'::regclass), pg_stat_get_dead_tuples('r10v{k}_3'::regclass), \
                 pg_stat_get_tuples_inserted('r10v{k}_4'::regclass), (SELECT count(*) FROM pg_stat_user_indexes WHERE {like}), \
                 (SELECT count(*) FROM pg_statio_user_tables WHERE {like})"
            ), true)),
        }
    }
    out.push(o(format!("DO $$ BEGIN FOR j IN 1..{n} LOOP EXECUTE format('DROP TABLE r10v{k}_%s', j); END LOOP; END $$")));
    out
}

impl super::Gen<'_> {
    /// Round 10's statements.
    pub(super) fn round10_stmt(&mut self) -> Vec<Stmt> {
        let rng = &mut self.rng;
        let k = rng.next() % 100_000;
        match rng.below(100) {
            0..=29 => nestloop(rng),
            30..=51 => plans(rng, k),
            52..=77 => batch(rng),
            78..=89 => queue(rng, k),
            _ => stats(rng, k),
        }
    }
}


