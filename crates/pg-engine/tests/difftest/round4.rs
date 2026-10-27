//! Round 4 of the grammar (seeds from `R4_SEEDS` on): a JSON / time / numeric table `tj4`,
//! a partitioned table `tp4` with sub-partitions and default partitions, and statements over
//! them: jsonpath and SQL/JSON functions (JSON_TABLE too), window frames (ROWS / GROUPS /
//! RANGE with offsets, EXCLUDE), LATERAL set-returning functions, MERGE and DML with
//! RETURNING old / new across partitions, numeric edge values, and timestamp / interval
//! arithmetic under session time zones.

use super::{Query, Rng, Stmt, TZS, Ty};
use super::Ty::*;

pub const JSON_DOCS: &[&str] = &[
    r#"{"a": [1, 2, 3], "b": {"c": "x"}, "s": "abc", "n": 2.5, "ts": "2025-03-09T01:30:00"}"#,
    r#"{"a": [{"b": 1}, {"b": "2"}, {"c": null}], "s": "Abc", "n": "10"}"#,
    r#"{"a": 5, "s": "", "ts": "2024-02-29"}"#,
    r#"[1, 2, {"a": 3}]"#,
    r#""str""#,
    "null",
    "{}",
    r#"{"a": [], "n": 1e3, "b": {"c": [1, {"d": true}]}}"#,
    r#"{"r": [{"x": 1, "y": "a"}, {"x": 2}, {"x": "q"}], "a": [4, 0, -1.5]}"#,
    r#"{"a": [10, 20, 30, 40], "s": "xyz", "n": -0.0, "ts": "2025-11-02 01:30:00+00"}"#,
    r#"{"a": [[1, 2], [3]], "b": {"c": "y", "e": null}, "n": 123456789012345678901234567890}"#,
    r#"{"a": ["1", "a", null, true], "s": "ß", "ts": "12:30:00"}"#,
];
const TSTZ: &[&str] = &[
    "2025-03-09 01:30:00-05", "2025-03-09 03:30:00-04", "2025-11-02 01:30:00-04", "2025-11-02 01:30:00-05",
    "2024-02-29 23:00:00+00", "1999-12-31 23:59:59.999999+00", "infinity", "-infinity", "0044-03-15 12:00:00+00 BC",
    "2025-10-26 00:30:00+00", "2025-10-26 01:30:00+00", "2038-01-19 03:14:08+00", "2025-04-06 02:30:00+11",
];
const TS: &[&str] = &[
    "2025-03-09 02:30:00", "2025-11-02 01:30:00", "2024-02-29 00:00:00", "2000-01-01 00:00:00", "infinity",
    "1970-01-01 00:00:00.5", "2025-10-26 02:30:00", "0001-01-01 00:00:00 BC", "2025-12-31 23:59:59.999999",
];
const DATES: &[&str] = &["2024-02-29", "2025-01-01", "2025-03-09", "infinity", "-infinity", "4713-01-01 BC", "2000-03-01", "1999-12-31"];
const INTERVALS: &[&str] = &[
    "1 day", "-1 mon 2 days", "23:59:59.999999", "1 year 2 mons", "P1DT12H", "-12:00", "0", "36 hours", "1 mon -30 days",
    "infinity", "-infinity", "2 weeks 3 microseconds", "-1 year -1 day",
];
const NUMERICS: &[&str] = &[
    "NaN", "Infinity", "-Infinity", "0", "-0.0", "1e30", "1.23456789012345678901234567890", "-1e-20", "123.45", "2.5",
    "-2.5", "0.000001", "99999999999999999999", "-7", "0.5",
];
const FLOATS: &[&str] = &["0.5", "-1.25", "2", "Infinity", "-0", "NaN", "1024", "0.125", "-3"];

fn pick_or_null(rng: &mut Rng, v: &[&str], quote: bool) -> String {
    if rng.chance(0.12) {
        return "NULL".into();
    }
    let x = rng.pick(v);
    if quote { format!("'{}'", x.replace('\'', "''")) } else { x.to_string() }
}

/// The tables: `tj4` (JSON documents, times, intervals, numeric and float edges) and `tp4`
/// (range-partitioned on k, with a list-partitioned partition on v, each level with a
/// default partition, a unique key over both).
pub fn schema(rng: &mut Rng) -> Vec<String> {
    let mut out = vec![
        "CREATE TABLE tj4 (id int PRIMARY KEY, j jsonb, js json, ts timestamptz, t timestamp, d date, iv interval, n numeric, f float8, k int)".to_string(),
        "CREATE TABLE tp4 (k int, d date, v text, n numeric) PARTITION BY RANGE (k)".into(),
        "CREATE TABLE tp4_lo PARTITION OF tp4 FOR VALUES FROM (MINVALUE) TO (0)".into(),
        "CREATE TABLE tp4_mid PARTITION OF tp4 FOR VALUES FROM (0) TO (10) PARTITION BY LIST (v)".into(),
        "CREATE TABLE tp4_mid_ab PARTITION OF tp4_mid FOR VALUES IN ('a', 'b')".into(),
        "CREATE TABLE tp4_mid_d PARTITION OF tp4_mid DEFAULT".into(),
        "CREATE TABLE tp4_hi PARTITION OF tp4 FOR VALUES FROM (10) TO (20)".into(),
        "CREATE TABLE tp4_def PARTITION OF tp4 DEFAULT".into(),
        "CREATE UNIQUE INDEX tp4_kv ON tp4 (k, v)".into(),
    ];
    let rows: Vec<String> = (1..=rng.range(4, 14))
        .map(|i| {
            let j = pick_or_null(rng, JSON_DOCS, true);
            let js = if rng.chance(0.3) { r#"'{"a": 1, "a": 2,  "b" : [1,2]}'"#.to_string() } else { j.clone() };
            format!(
                "({i}, {j}, {js}, {}, {}, {}, {}, {}, {}, {})",
                pick_or_null(rng, TSTZ, true),
                pick_or_null(rng, TS, true),
                pick_or_null(rng, DATES, true),
                pick_or_null(rng, INTERVALS, true),
                pick_or_null(rng, NUMERICS, true),
                pick_or_null(rng, FLOATS, true),
                if rng.chance(0.15) { "NULL".to_string() } else { rng.range(-3, 12).to_string() },
            )
        })
        .collect();
    out.push(format!("INSERT INTO tj4 VALUES {}", rows.join(", ")));
    let mut seen = Vec::new();
    let mut prow = Vec::new();
    for _ in 0..rng.range(3, 16) {
        let k = rng.range(-5, 25);
        let v = pick_or_null(rng, &["a", "b", "c", "d", "ab"], true);
        if seen.contains(&(k, v.clone())) && v != "NULL" {
            continue;
        }
        seen.push((k, v.clone()));
        prow.push(format!("({k}, {}, {v}, {})", pick_or_null(rng, DATES, true), pick_or_null(rng, NUMERICS, true)));
    }
    out.push(format!("INSERT INTO tp4 VALUES {}", prow.join(", ")));
    if rng.chance(0.5) {
        out.push("ANALYZE tj4, tp4".into());
    }
    out
}

const PATHS: &[&str] = &[
    "$.a", "$.a[*] ? (@ > $x)", "strict $.a[0]", "lax $.**.b", "$.a.size()", "$.a.type()", "$.*.keyvalue()",
    "$.ts.datetime()", "$.a[last]", "$.a[1 to 2]", r#"$ ? (@.s like_regex "^a" flag "i")"#, "$.n.double()",
    "$.n.ceiling()", "$.a[*] ? (exists(@.b))", "$.s.string()", "$.a[*].abs()", r#"$.s ? (@ starts with "a")"#,
    "$.a[*] ? (@ == $x || @ == \"1\")", "strict $.b.c", "$.b.*", "$.r[*].x", "$.a[*] + 1", "$.n * 2",
    r#"$.ts.datetime("YYYY-MM-DD")"#, "$.a[*] ? (@.type() == \"number\")", "$.a ? (@.size() > 2)", "$.**{2}",
    "$.a[$x]", "$.n.bigint()", "$.n.decimal(6, 2)", "$.ts.timestamp()", "$.n.integer()", "$.a.boolean()",
];

fn path(rng: &mut Rng) -> String {
    rng.pick(PATHS).replace('\'', "''")
}

fn vars(rng: &mut Rng) -> String {
    format!(r#"'{{"x": {}}}'"#, rng.range(0, 3))
}

impl super::Gen<'_> {
    /// One round 4 statement (with a SET TIME ZONE before a time query now and then).
    pub(super) fn round4_stmt(&mut self) -> Vec<Stmt> {
        let rng = &mut self.rng;
        let q = |sql: String, ordered: bool, cols: &[Ty]| {
            Stmt::Query(Query { sql, ordered, cols: cols.iter().enumerate().map(|(i, t)| (format!("c{i}"), *t)).collect() })
        };
        let mut out = Vec::new();
        let p = path(rng);
        let x = vars(rng);
        let silent = *rng.pick(&["", ", silent => true", ", silent => false"]);
        let stmt = match rng.below(14) {
            // jsonpath functions and operators.
            0 => {
                let f = *rng.pick(&["jsonb_path_query_array", "jsonb_path_query_first", "jsonb_path_exists", "jsonb_path_match"]);
                q(format!("SELECT id, {f}(j, '{p}', {x}{silent}) FROM tj4 ORDER BY id"), true, &[Int, Jsonb])
            }
            1 => {
                let op = *rng.pick(&["@?", "@@"]);
                let w = *rng.pick(&["", "_tz"]);
                q(format!("SELECT id, j {op} '{p}', jsonb_path_query_array{w}(j, '{p}', {x}, true) FROM tj4 WHERE j IS NOT NULL ORDER BY id"), true, &[Int, Bool, Jsonb])
            }
            2 => q(format!("SELECT t.id, r.v FROM tj4 t, LATERAL jsonb_path_query(t.j, '{p}', {x}, true) WITH ORDINALITY r(v, o) ORDER BY t.id, r.o"), false, &[Int, Jsonb]),
            // SQL/JSON query functions (non-NULL inputs: PostgreSQL 18.6 carries a row's ON
            // ERROR default into a later NULL input's result).
            3 => {
                let ret = *rng.pick(&["", " RETURNING int", " RETURNING text", " RETURNING numeric", " RETURNING date", " RETURNING jsonb", " RETURNING bool"]);
                let on = *rng.pick(&["", " NULL ON ERROR", " ERROR ON ERROR", " DEFAULT -1 ON ERROR", " DEFAULT 0 ON EMPTY", " NULL ON EMPTY ERROR ON ERROR"]);
                let on = if ret.contains("date") || ret.contains("bool") || ret.contains("jsonb") { on.replace("-1", "NULL").replace("DEFAULT 0", "NULL") } else { on.to_string() };
                q(format!("SELECT id, JSON_VALUE(j, '{p}' PASSING 1 AS x{ret}{on}) FROM tj4 WHERE j IS NOT NULL ORDER BY id"), true, &[Int, Text])
            }
            4 => {
                let wrap = *rng.pick(&["", " WITH WRAPPER", " WITH CONDITIONAL WRAPPER", " WITHOUT WRAPPER", " WITH UNCONDITIONAL ARRAY WRAPPER"]);
                let quotes = if wrap.is_empty() || wrap.contains("WITHOUT") { *rng.pick(&["", " KEEP QUOTES", " OMIT QUOTES"]) } else { "" };
                let on = *rng.pick(&["", " EMPTY ARRAY ON EMPTY", " NULL ON ERROR", " ERROR ON ERROR", " EMPTY OBJECT ON ERROR"]);
                let ret = *rng.pick(&["", " RETURNING text", " RETURNING json", " RETURNING int[]"]);
                q(format!("SELECT id, JSON_QUERY(j, '{p}' PASSING 2 AS x{ret}{wrap}{quotes}{on}) FROM tj4 WHERE j IS NOT NULL ORDER BY id"), true, &[Int, Text])
            }
            5 => {
                let on = *rng.pick(&["", " TRUE ON ERROR", " ERROR ON ERROR", " UNKNOWN ON ERROR"]);
                q(format!("SELECT id, JSON_EXISTS(j, '{p}' PASSING 1 AS x{on}), j IS JSON OBJECT, js IS JSON WITH UNIQUE KEYS, js::text IS JSON ARRAY FROM tj4 ORDER BY id"), true, &[Int, Bool, Bool, Bool, Bool])
            }
            6 => {
                let cols = *rng.pick(&[
                    "o FOR ORDINALITY, x int PATH '$', y text PATH '$.b', z jsonb PATH '$' WITH WRAPPER, e boolean EXISTS PATH '$.c'",
                    "o FOR ORDINALITY, x numeric PATH '$' DEFAULT 0 ON ERROR, NESTED PATH '$[*]' COLUMNS (y int PATH '$' NULL ON ERROR)",
                    "x text PATH '$' OMIT QUOTES, t text FORMAT JSON PATH '$', o FOR ORDINALITY",
                    "o FOR ORDINALITY, NESTED PATH '$.b' COLUMNS (b text PATH '$'), NESTED PATH '$.c' COLUMNS (c jsonb PATH '$')",
                ]);
                let root = *rng.pick(&["$.a[*]", "$.r[*]", "$[*]", "strict $.a[*]", "$.b"]);
                let err = *rng.pick(&["", " ERROR ON ERROR", " EMPTY ARRAY ON ERROR"]);
                q(format!("SELECT t.id, jt.* FROM tj4 t, JSON_TABLE(t.j, '{root}' COLUMNS ({cols}){err}) jt ORDER BY 1, 2"), false, &[Int, Int, Text])
            }
            7 => {
                let agg = *rng.pick(&[
                    "JSON_ARRAYAGG(n ORDER BY id)",
                    "JSON_ARRAYAGG(j ORDER BY id NULL ON NULL RETURNING jsonb)",
                    "JSON_OBJECTAGG(id::text VALUE k ABSENT ON NULL)",
                    "JSON_OBJECTAGG(coalesce(k, 0)::text : id WITH UNIQUE KEYS RETURNING jsonb)",
                    "jsonb_agg(j -> 'a' ORDER BY id)",
                    "jsonb_object_agg(id, ts ORDER BY id)",
                    "json_agg(js ORDER BY id)::text",
                ]);
                let g = *rng.pick(&["", " GROUP BY k % 2 ORDER BY k % 2"]);
                q(format!("SELECT {agg} FROM tj4{g}"), !g.is_empty(), &[Text])
            }
            8 => {
                let e = *rng.pick(&[
                    "JSON_OBJECT('a' : k, 'b' VALUE n, 'c' : NULL ABSENT ON NULL)",
                    "JSON_ARRAY(k, n, ts NULL ON NULL)",
                    "JSON_SERIALIZE(j RETURNING text)",
                    "JSON_SCALAR(n)",
                    "JSON(js::text WITH UNIQUE KEYS)",
                    "jsonb_set(j, '{a,0}', to_jsonb(k), true)",
                    "jsonb_insert(j, '{a,1}', '\"ins\"')",
                    "jsonb_strip_nulls(j, true)",
                    "j #- '{a,0}'",
                    "(j - 'a') || '{\"z\": 1}'",
                    "j ?| ARRAY['a', 's']",
                    "jsonb_typeof(j -> 'a') || jsonb_typeof(j)",
                    "to_jsonb(ts) || to_jsonb(iv) || to_jsonb(n) || to_jsonb(f)",
                    "js -> 'a'",
                    "json_typeof(js)",
                ]);
                q(format!("SELECT id, {e} FROM tj4 ORDER BY id"), true, &[Int, Text])
            }
            // Window frames.
            9 => {
                let (key, off) = *rng.pick(&[("k", "2"), ("n", "1.5"), ("ts", "'1 day'::interval"), ("d", "'7 days'::interval"), ("iv", "'12 hours'::interval"), ("f", "0.5::float8"), ("k", "0")]);
                let dir = *rng.pick(&["", " DESC", " NULLS FIRST", " DESC NULLS LAST"]);
                let ex = *rng.pick(&["", " EXCLUDE CURRENT ROW", " EXCLUDE GROUP", " EXCLUDE TIES", " EXCLUDE NO OTHERS"]);
                let (frame, total) = match rng.below(5) {
                    0 => (format!("RANGE BETWEEN {off} PRECEDING AND {off} FOLLOWING"), false),
                    1 => (format!("RANGE BETWEEN UNBOUNDED PRECEDING AND {off} FOLLOWING"), false),
                    2 => (format!("GROUPS BETWEEN {} PRECEDING AND CURRENT ROW", rng.range(0, 2)), false),
                    3 => (format!("GROUPS BETWEEN CURRENT ROW AND {} FOLLOWING", rng.range(1, 3)), false),
                    _ => (format!("ROWS BETWEEN {} PRECEDING AND {} FOLLOWING", rng.range(0, 2), rng.range(0, 2)), true),
                };
                let order = if total { format!("{key}{dir}, id") } else { format!("{key}{dir}") };
                let f = *rng.pick(&["count(*)", "sum(k)", "max(n)", "min(ts)", "sum(f)", "count(j)", "bool_or(k > 3)", "max(iv)"]);
                let extra = if total { ", first_value(id) OVER w, nth_value(id, 2) OVER w, last_value(id) OVER w" } else { "" };
                q(format!("SELECT id, {f} OVER w{extra} FROM tj4 WINDOW w AS (ORDER BY {order} {frame}{ex}) ORDER BY id"), true, &[Int, Big])
            }
            // LATERAL set-returning functions.
            10 => {
                let from = *rng.pick(&[
                    "LATERAL jsonb_array_elements(CASE WHEN jsonb_typeof(t.j -> 'a') = 'array' THEN t.j -> 'a' END) WITH ORDINALITY e(v, o)",
                    "LEFT JOIN LATERAL jsonb_each(CASE WHEN jsonb_typeof(t.j) = 'object' THEN t.j END) e(v, o) ON true",
                    "LATERAL generate_series(CASE WHEN isfinite(t.ts) THEN t.ts END, t.ts + interval '1 day', interval '7 hours') WITH ORDINALITY e(v, o)",
                    "LATERAL unnest(ARRAY[t.k, t.id], ARRAY['x', 'y', 'z']) e(v, o)",
                    "LATERAL ROWS FROM (generate_series(1, least(t.k, 4)), jsonb_array_elements_text(CASE WHEN jsonb_typeof(t.j -> 'a') = 'array' THEN t.j -> 'a' END)) e(v, o)",
                    "LATERAL jsonb_to_recordset(CASE WHEN jsonb_typeof(t.j -> 'r') = 'array' THEN t.j -> 'r' END) e(v int, o text)",
                    "LATERAL (SELECT p.v AS v, p.k AS o FROM tp4 p WHERE p.k > t.k ORDER BY p.k, p.v LIMIT 2) e",
                    "LEFT JOIN LATERAL generate_series(CASE WHEN isfinite(t.d) THEN t.d END, t.d + 3, '1 day') e(v) ON t.k > 0 CROSS JOIN LATERAL (SELECT e.v IS NULL) o(o)",
                    "LATERAL regexp_matches(coalesce(t.j ->> 's', 'aXbX'), '([a-z])', 'gi') WITH ORDINALITY e(v, o)",
                    "LATERAL string_to_table(t.js::text, ',') WITH ORDINALITY e(v, o)",
                ]);
                q(format!("SELECT t.id, e.v::text, e.o::text FROM tj4 t {from}"), false, &[Int, Text, Text])
            }
            11 => q(format!(
                "SELECT id, generate_series(1, least(k, 3)), {} FROM tj4 ORDER BY 1, 2",
                rng.pick(&["jsonb_array_elements_text(CASE WHEN jsonb_typeof(j -> 'a') = 'array' THEN j -> 'a' END)", "unnest(ARRAY[n, -n])", "generate_series(CASE WHEN isfinite(ts) THEN ts END, ts + least(iv, interval '3 days'), '1 day')"])
            ), false, &[Int, Int, Text]),
            // Partitions, MERGE and DML with RETURNING.
            12 => {
                let (k1, k2) = (rng.range(-5, 25), rng.range(-5, 25));
                let (v1, v2) = (pick_or_null(rng, &["a", "b", "c", "x"], true), pick_or_null(rng, &["a", "d"], true));
                let shift = rng.range(-10, 10);
                let sql = match rng.below(6) {
                    0 => format!(
                        "MERGE INTO tp4 t USING (VALUES ({k1}, {v1}, 1.5), ({k2}, {v2}, -2)) s(k, v, n) ON t.k = s.k AND t.v IS NOT DISTINCT FROM s.v \
                         WHEN MATCHED AND s.n > 0 THEN UPDATE SET n = t.n + s.n, k = t.k + {shift} WHEN MATCHED THEN DELETE \
                         WHEN NOT MATCHED THEN INSERT VALUES (s.k, DATE '2025-01-01', s.v, s.n) RETURNING merge_action(), old.k, new.k, new.v, tableoid::regclass::text"
                    ),
                    1 => format!(
                        "MERGE INTO tp4 t USING (SELECT k, id FROM tj4 WHERE k IS NOT NULL) s ON t.k = s.k \
                         WHEN MATCHED AND t.v IS NULL THEN DELETE WHEN NOT MATCHED BY SOURCE AND t.k > {k1} THEN UPDATE SET v = 'z' \
                         WHEN NOT MATCHED BY TARGET THEN DO NOTHING RETURNING merge_action(), old.v, new.v, t.k"
                    ),
                    2 => format!("UPDATE tp4 SET k = k + {shift}, v = upper(v) WHERE k BETWEEN {} AND {} RETURNING old.k, new.k, old.v, new.v, tableoid::regclass::text", k1.min(k2), k1.max(k2)),
                    3 => format!("INSERT INTO tp4 VALUES ({k1}, '2024-02-29', {v1}, 7), ({k2}, NULL, {v2}, NULL) ON CONFLICT (k, v) DO UPDATE SET n = coalesce(tp4.n, 0) + 1 RETURNING old.n, new.n, k, tableoid::regclass::text"),
                    4 => format!("DELETE FROM tp4 WHERE k > {k1} AND (v IS NULL OR v > {v2}) RETURNING old.*, tableoid::regclass::text"),
                    _ => format!("UPDATE tj4 SET n = n * 2, ts = ts + iv, k = k + {shift} WHERE k > {k1} RETURNING id, old.n, new.n, new.ts"),
                };
                let table = if sql.contains("tj4 SET") { "tj4" } else { "tp4" };
                Stmt::Dml { sql, table: table.into(), ordered: false }
            }
            13 => {
                let w = *rng.pick(&[
                    "k BETWEEN 0 AND 9", "v = 'c'", "k >= 20 OR k < 0", "v IS NULL", "k = 5 AND v IN ('a', 'z')", "k IN (-1, 10, 25)",
                    "d > '2024-01-01'", "true",
                ]);
                let s = *rng.pick(&[
                    "tableoid::regclass::text, k, v",
                    "k % 3, count(*), sum(n), max(v)",
                    "v, count(*) FILTER (WHERE n > 0), min(d)",
                ]);
                let g = if s.contains("count") { " GROUP BY 1 ORDER BY 1".to_string() } else { " ORDER BY 1, 2, 3".to_string() };
                q(format!("SELECT {s} FROM tp4 WHERE {w}{g}"), false, &[Text, Int, Text])
            }
            // Numeric edges and times under a time zone.
            _ if rng.chance(0.5) => {
                let e = *rng.pick(&[
                    "round(n, 3), trunc(n, -1)", "n / 3, mod(n, 7)", "n ^ 2", "sqrt(abs(n))", "ln(abs(n) + 1), exp(least(n, 50))",
                    "scale(n), min_scale(n), trim_scale(n)", "n::float8, f::numeric", "n::int", "to_char(n, 'S9G999D99'), to_char(n, '9.99EEEE')",
                    "width_bucket(n, 0, 100, 5)", "div(n, 3), n % 2.5", "round(f::numeric, 1), floor(n), ceil(n)", "log(10, abs(n) + 2)",
                    "n::numeric(10, 2)", "power(n, 0.5)", "sum(n) OVER (), avg(n) OVER ()", "n < f::numeric, greatest(n, 0), least(n, 0)",
                    "factorial(least(abs(k), 20)), gcd(k, 6), lcm(k, 4)", "numeric_inc(n), abs(n), sign(n)",
                ]);
                q(format!("SELECT id, {e} FROM tj4 ORDER BY id"), true, &[Int, Num])
            }
            _ => {
                if rng.chance(0.5) {
                    out.push(Stmt::Other(format!("SET TIME ZONE '{}'", rng.pick(TZS))));
                }
                let e = *rng.pick(&[
                    "ts + iv, ts - iv", "ts + interval '1 day', ts + interval '24 hours'", "ts AT TIME ZONE 'America/New_York', t AT TIME ZONE 'Europe/Berlin'",
                    "date_trunc('day', ts), date_trunc('month', ts, 'Asia/Kolkata')", "extract(epoch FROM ts), extract(isodow FROM t)",
                    "age(ts, t::timestamptz), age(t)", "justify_interval(iv * 1.5), iv / 3", "t - ts::timestamp", "ts::date, ts::time, d + iv",
                    "date_bin('15 minutes', ts, '2001-01-01'), date_bin('1 day', t, '2000-01-01 12:00')", "to_char(ts, 'YYYY-MM-DD HH24:MI:SS TZ OF')",
                    "make_timestamptz(2025, 3, 9, 2, 30, 0), make_timestamptz(2025, 11, 2, 1, 30, 0, 'America/New_York')", "ts < t, ts = t::timestamptz",
                    "timezone('UTC', t), t::timestamptz", "d - 1, d + 1, d - '2000-01-01'::date", "iv * k, iv < interval '1 day'",
                    "extract(hour FROM ts AT TIME ZONE 'Australia/Lord_Howe'), extract(timezone FROM ts)", "isfinite(ts), isfinite(iv)",
                    "ts::text, iv::text, t::text", "date_part('day', iv), justify_days(iv), justify_hours(iv)",
                ]);
                out.push(q(format!("SELECT id, {e} FROM tj4 ORDER BY id"), true, &[Int, Tstz]));
                if rng.chance(0.5) {
                    out.push(Stmt::Other("RESET TimeZone".into()));
                }
                return out;
            }
        };
        out.push(stmt);
        out
    }
}
