//! Round 7 of the grammar (seeds from `R7_SEEDS` on), on the shared engine with sessions 2 and
//! 3 over the wire (`difftest/wire.rs`): what applications send that the suites don't cover.
//!
//! - Statements that differ only in their literals (the engine keeps simple queries' parse
//!   trees and plans per template): the same template with literals of other values and
//!   types (`1`, `2147483648`, `1.5`, `'3'`, NULL), in ORDER BY / GROUP BY positions, LIMIT,
//!   LIKE patterns and EXPLAIN, from session 1 (in-process), 2 and 3 (simple protocol), and
//!   over the extended protocol with the literals as parameters, typed or not, text or
//!   binary, results in either format, named statements executed past the custom plans.
//! - Savepoints and errors inside transactions, and the transaction's state after them.
//! - Two sessions interleaved, deterministic by construction: row locks waited for and
//!   re-checked (EvalPlanQual), SKIP LOCKED, NOWAIT, lock_timeout, table locks, advisory
//!   locks, rows moved to another partition meanwhile, unique keys inserted concurrently,
//!   deadlocks (found by session 3 alone: its deadlock_timeout is the short one), serializable
//!   write skew.
//! - MERGE with every action, partitioned tables with rows moving between partitions,
//!   generated (stored, virtual) and identity columns, domains with checks and arrays of them,
//!   ICU and nondeterministic collations, time zones and DST edges, numeric and float edges,
//!   GRANT / REVOKE and row security under several roles.
//!
//! Its objects are named `r7*`; the roles `r7_alice` and `r7_bob` are the cluster's (made if
//! missing, never dropped: other seeds' databases may hold their grants).

use super::{Gen, Query, Rng, Stmt, Ty};

fn query(sql: String, ordered: bool) -> Stmt {
    Stmt::Query(Query { sql, ordered, cols: Vec::<(String, Ty)>::new() })
}
fn dml(sql: String, table: &str) -> Stmt {
    Stmt::Dml { sql, table: table.into(), ordered: false }
}
fn o(s: impl Into<String>) -> Stmt {
    Stmt::Other(s.into())
}

/// A literal: as SQL text, and as a parameter (its type's oid, 0 when the server is to infer
/// it, its text, and its binary form when the generator knows it).
#[derive(Clone)]
struct Lit {
    sql: String,
    oid: u32,
    text: Option<String>,
    bin: Option<Vec<u8>>,
}

fn lit(sql: &str, oid: u32, text: Option<&str>, bin: Option<Vec<u8>>) -> Lit {
    Lit { sql: sql.into(), oid, text: text.map(String::from), bin }
}
fn int4(v: i32) -> Lit {
    lit(&v.to_string(), 23, Some(&v.to_string()), Some(v.to_be_bytes().to_vec()))
}
fn int8(v: i64) -> Lit {
    lit(&v.to_string(), 20, Some(&v.to_string()), Some(v.to_be_bytes().to_vec()))
}
fn text(v: &str) -> Lit {
    lit(&format!("'{}'", v.replace('\'', "''")), 25, Some(v), Some(v.as_bytes().to_vec()))
}
fn null(oid: u32) -> Lit {
    lit("NULL", oid, None, None)
}

/// numeric_send's form of a decimal (or NaN / Infinity / -Infinity).
fn numeric_bin(v: &str) -> Vec<u8> {
    let special = match v {
        "NaN" => Some(0xC000u16),
        "Infinity" => Some(0xD000),
        "-Infinity" => Some(0xF000),
        _ => None,
    };
    let mut out = Vec::new();
    if let Some(sign) = special {
        for x in [0u16, 0, sign, 0] {
            out.extend_from_slice(&x.to_be_bytes());
        }
        return out;
    }
    let (neg, v) = v.strip_prefix('-').map_or((false, v), |r| (true, r));
    let (int, frac) = v.split_once('.').unwrap_or((v, ""));
    let int = int.trim_start_matches('0');
    // Base-10000 digits: the integer part padded on the left, the fraction on the right.
    let ipad = format!("{}{int}", "0".repeat((4 - int.len() % 4) % 4));
    let fpad = format!("{frac}{}", "0".repeat((4 - frac.len() % 4) % 4));
    let mut digits: Vec<u16> = ipad.as_bytes().chunks(4).chain(fpad.as_bytes().chunks(4)).map(|c| std::str::from_utf8(c).unwrap().parse().unwrap()).collect();
    let mut weight = ipad.len() as i16 / 4 - 1;
    while digits.first() == Some(&0) {
        digits.remove(0);
        weight -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        weight = 0;
    }
    let sign = if neg && !digits.is_empty() { 0x4000u16 } else { 0 };
    for x in [digits.len() as u16, weight as u16, sign, frac.len() as u16] {
        out.extend_from_slice(&x.to_be_bytes());
    }
    for d in digits {
        out.extend_from_slice(&d.to_be_bytes());
    }
    out
}
fn numeric(v: &str) -> Lit {
    let sql = if v.parse::<f64>().is_ok() && !v.contains(['N', 'I']) { v.to_string() } else { format!("'{v}'") };
    lit(&sql, 1700, Some(v), Some(numeric_bin(v)))
}

/// Days from 2000-01-01 (date_send), proleptic Gregorian.
fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468 - 10957
}
fn date(y: i64, m: i64, d: i64) -> Lit {
    let s = format!("{y:04}-{m:02}-{d:02}");
    lit(&format!("'{s}'"), 1082, Some(&s), Some((days(y, m, d) as i32).to_be_bytes().to_vec()))
}
/// A timestamptz in UTC (timestamptz_send: microseconds from 2000-01-01 00:00 UTC).
fn tstz(y: i64, mo: i64, d: i64, h: i64, mi: i64) -> Lit {
    let s = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:00+00");
    let us = ((days(y, mo, d) * 24 + h) * 60 + mi) * 60_000_000;
    lit(&format!("'{s}'"), 1184, Some(&s), Some(us.to_be_bytes().to_vec()))
}
fn interval(us: i64, days: i32, months: i32, text: &str) -> Lit {
    let mut b = us.to_be_bytes().to_vec();
    b.extend_from_slice(&days.to_be_bytes());
    b.extend_from_slice(&months.to_be_bytes());
    lit(&format!("'{text}'"), 1186, Some(text), Some(b))
}
/// A one-dimensional int4[] (array_send), NULL elements as None.
fn int4_array(v: &[Option<i32>]) -> Lit {
    let mut b = Vec::new();
    for x in [1i32, v.iter().any(Option::is_none) as i32, 23, v.len() as i32, 1] {
        b.extend_from_slice(&x.to_be_bytes());
    }
    for x in v {
        match x {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(x) => {
                b.extend_from_slice(&4i32.to_be_bytes());
                b.extend_from_slice(&x.to_be_bytes());
            }
        }
    }
    let t: Vec<String> = v.iter().map(|x| x.map_or("NULL".into(), |x| x.to_string())).collect();
    let t = format!("{{{}}}", t.join(","));
    lit(&format!("'{t}'"), 1007, Some(&t), Some(b))
}

/// A literal for a template's slot: `k` a key of r7k, `g` a group, `i` an int, `x` a literal
/// of another type each time (int, bigint, numeric, unknown, NULL), `t` text, `p` a LIKE
/// pattern, `n` numeric, `d` a date, `z` a time zone, `s` a small int, `r` a region, `a` an
/// int4[], `v` an interval, `w` a timestamptz.
fn slot(rng: &mut Rng, kind: char) -> Lit {
    match kind {
        'k' => int4(*rng.pick(&[1, 2, 3, 4, 5, 6, 7, 8, 11, 0, 99, -1])),
        'g' => int4(*rng.pick(&[1, 1, 2, 3, 9, 99])),
        'i' => int4(*rng.pick(&[0, 1, -1, 7, 42, 100, 2147483647, -2147483648])),
        's' => int4(*rng.pick(&[-2, 0, 1, 2, 3, 5])),
        'x' => match rng.below(10) {
            0 => int4(3),
            1 => int8(2147483648),
            2 => numeric("1.5"),
            3 => numeric("2.0"),
            4 => lit("'4'", 0, Some("4"), None),
            5 => null(23),
            6 => int8(-9223372036854775808),
            7 => numeric("9223372036854775808"),
            8 => lit("' 5 '", 0, Some(" 5 "), None),
            _ => int4(-0),
        },
        't' => match rng.below(9) {
            0 => text("a"),
            1 => text("it's"),
            2 => text(""),
            3 => text("A%"),
            4 => text("é"),
            5 => lit("E'x\\ty'", 25, Some("x\ty"), Some(b"x\ty".to_vec())),
            6 => lit("$$z$$", 25, Some("z"), Some(b"z".to_vec())),
            7 => text("NULL"),
            _ => text("note 3"),
        },
        'p' => text(rng.pick(&["n%", "%3", "note _", "%", "", "N%", "note\\_%", "x%"])),
        'n' => numeric(rng.pick(&["0.5", "-1.25", "123456789.123", "0.001", "NaN", "-Infinity", "0", "99999999999999999999.5"])),
        'd' => match rng.below(6) {
            0 => date(2025, 3, 9),
            1 => date(2024, 2, 29),
            2 => date(2025, 11, 2),
            3 => lit("'infinity'", 1082, Some("infinity"), Some(i32::MAX.to_be_bytes().to_vec())),
            4 => date(1999, 12, 31),
            _ => null(1082),
        },
        'z' => text(rng.pick(&["UTC", "America/New_York", "Europe/Berlin", "Australia/Lord_Howe", "Asia/Kathmandu", "+05:30", "EST5EDT", "Pacific/Chatham"])),
        'r' => match rng.below(5) {
            0 | 1 => text("eu"),
            2 => text("us"),
            3 => text("xx"),
            _ => null(25),
        },
        'a' => int4_array(rng.pick(&[&[Some(1), Some(2)][..], &[], &[Some(3), None], &[Some(1), Some(1), Some(99)]])),
        'v' => match rng.below(4) {
            0 => interval(0, 1, 0, "1 day"),
            1 => interval(3_600_000_000, 0, 0, "01:00:00"),
            2 => interval(0, 0, 1, "1 mon"),
            _ => interval(-1, 0, 0, "-00:00:00.000001"),
        },
        _ => match rng.below(5) {
            0 => tstz(2025, 3, 9, 7, 30),
            1 => tstz(2025, 11, 2, 5, 30),
            2 => tstz(2025, 3, 30, 1, 0),
            3 => tstz(2000, 1, 1, 0, 0),
            _ => tstz(2025, 10, 26, 0, 59),
        },
    }
}

const NOTES: &[&str] = &["note 1", "note 2", "Note 3", "note_4", "n%5", "", "note 3", "é", "NOTE 3"];

/// The objects: keyed rows `r7k` (skewed groups, analyzed or not), partitioned `r7p` (list,
/// a default partition or not) and `r7r` (range), generated and identity columns `r7g`,
/// domains `r7d`, collated text `r7t`, and `r7acct` under row security with grants to the
/// roles.
pub fn schema(rng: &mut Rng) -> Vec<String> {
    let mut out: Vec<String> = vec![
        "DO $$ BEGIN CREATE ROLE r7_alice; EXCEPTION WHEN duplicate_object THEN NULL; END $$".into(),
        "DO $$ BEGIN CREATE ROLE r7_bob; EXCEPTION WHEN duplicate_object THEN NULL; END $$".into(),
        "CREATE DOMAIN r7pos AS int CHECK (VALUE > 0)".into(),
        "CREATE DOMAIN r7code AS text CHECK (VALUE ~ '^[A-Z]{2}[0-9]$')".into(),
        "CREATE DOMAIN r7money AS numeric(12, 2) NOT NULL CHECK (VALUE >= -1000)".into(),
        "CREATE COLLATION r7ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false)".into(),
        "CREATE COLLATION r7de (provider = icu, locale = 'de-u-co-phonebk')".into(),
        "CREATE TABLE r7k (id int PRIMARY KEY, grp int NOT NULL, v int, note text, amt numeric(12, 3), f float8, ts timestamptz)".into(),
        "CREATE INDEX r7k_grp ON r7k (grp)".into(),
        "CREATE INDEX r7k_note ON r7k (note text_pattern_ops)".into(),
    ];
    let n = rng.range(8, 40);
    let mut rows = Vec::new();
    for id in 1..=n {
        let grp = if rng.chance(0.7) { 1 } else { rng.range(2, 9) };
        let v = if rng.chance(0.1) { "NULL".to_string() } else { rng.range(-50, 200).to_string() };
        let note = *rng.pick(NOTES);
        let amt = *rng.pick(&["0.5", "-12.125", "1000", "NULL", "3.333", "0"]);
        let f = *rng.pick(&["0.5", "-0", "1e300", "'NaN'", "'Infinity'", "2.25", "NULL"]);
        let ts = *rng.pick(&["'2025-03-09 06:59:59+00'", "'2025-03-09 07:00:00+00'", "'2025-11-02 05:30:00+00'", "'2024-02-29 12:00:00+00'", "NULL"]);
        rows.push(format!("({id}, {grp}, {v}, '{note}', {amt}, {f}, {ts})"));
    }
    out.push(format!("INSERT INTO r7k VALUES {}", rows.join(", ")));
    out.push("CREATE TABLE r7p (id int NOT NULL, region text NOT NULL, qty int, PRIMARY KEY (id, region)) PARTITION BY LIST (region)".into());
    out.push("CREATE TABLE r7p_eu PARTITION OF r7p FOR VALUES IN ('eu')".into());
    out.push("CREATE TABLE r7p_us PARTITION OF r7p FOR VALUES IN ('us')".into());
    if rng.chance(0.6) {
        out.push("CREATE TABLE r7p_def PARTITION OF r7p DEFAULT".into());
    }
    let rows: Vec<String> = (1..=rng.range(3, 10)).map(|i| format!("({i}, '{}', {})", if i % 3 == 0 { "us" } else { "eu" }, i * 7 % 23)).collect();
    out.push(format!("INSERT INTO r7p VALUES {}", rows.join(", ")));
    out.push("CREATE TABLE r7r (id int NOT NULL, qty int NOT NULL, tag text, PRIMARY KEY (id, qty)) PARTITION BY RANGE (qty)".into());
    out.push("CREATE TABLE r7r_neg PARTITION OF r7r FOR VALUES FROM (MINVALUE) TO (0)".into());
    out.push("CREATE TABLE r7r_low PARTITION OF r7r FOR VALUES FROM (0) TO (100)".into());
    out.push("CREATE TABLE r7r_high PARTITION OF r7r FOR VALUES FROM (100) TO (MAXVALUE)".into());
    out.push("INSERT INTO r7r VALUES (1, -5, 'a'), (2, 5, 'b'), (3, 50, NULL), (4, 150, 'd'), (5, 99, 'e')".into());
    let ident = if rng.chance(0.5) { "ALWAYS" } else { "BY DEFAULT" };
    out.push(format!("CREATE TABLE r7g (id int GENERATED {ident} AS IDENTITY (START WITH 10 INCREMENT BY 5) PRIMARY KEY, a int, t text, b int GENERATED ALWAYS AS (a * 2) STORED, c text GENERATED ALWAYS AS (upper(t) || a) VIRTUAL)"));
    out.push("INSERT INTO r7g (a, t) VALUES (1, 'x'), (NULL, 'y'), (3, NULL), (1073741824, 'big')".into());
    out.push("CREATE TABLE r7d (id int PRIMARY KEY, p r7pos, ps r7pos[], code r7code, m r7money DEFAULT 0)".into());
    out.push("INSERT INTO r7d VALUES (1, 1, '{1,2}', 'AB1', 5), (2, NULL, '{}', NULL, -1000), (3, 7, NULL, 'ZZ9', 12.5)".into());
    out.push("CREATE TABLE r7t (id int PRIMARY KEY, s text, c text COLLATE r7ci)".into());
    let words = ["a", "A", "b", "ß", "ss", "Æ", "ae", "ä", "Ä", "z", "é", "e", "E", "10", "9", "ä b", ""];
    let rows: Vec<String> = (1..=rng.range(5, 14)).map(|i| { let w = *rng.pick(&words); format!("({i}, '{w}', '{w}')") }).collect();
    out.push(format!("INSERT INTO r7t VALUES {}", rows.join(", ")));
    out.push("CREATE TABLE r7acct (id int PRIMARY KEY, owner text NOT NULL DEFAULT current_user, amount int, region text)".into());
    out.push("INSERT INTO r7acct VALUES (1, 'r7_alice', 10, 'eu'), (2, 'r7_bob', 20, 'us'), (3, 'postgres', 30, 'eu'), (4, 'r7_alice', -5, 'xx'), (5, 'r7_bob', NULL, NULL)".into());
    out.push("ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY".into());
    if rng.chance(0.3) {
        out.push("ALTER TABLE r7acct FORCE ROW LEVEL SECURITY".into());
    }
    out.push("CREATE POLICY r7_own ON r7acct USING (owner = current_user)".into());
    out.push("CREATE POLICY r7_ins ON r7acct FOR INSERT WITH CHECK (amount >= 0)".into());
    if rng.chance(0.5) {
        out.push("CREATE POLICY r7_region ON r7acct AS RESTRICTIVE USING (region IS DISTINCT FROM 'xx')".into());
    }
    if rng.chance(0.4) {
        out.push("CREATE POLICY r7_bob_all ON r7acct TO r7_bob USING (true) WITH CHECK (region = 'us')".into());
    }
    out.push("GRANT SELECT, INSERT, UPDATE (amount, region) ON r7acct TO r7_alice".into());
    out.push(format!("GRANT {} ON r7acct TO r7_bob", rng.pick(&["SELECT", "SELECT, DELETE", "ALL"])));
    out.push(format!("GRANT SELECT ({}) ON r7k TO r7_alice", rng.pick(&["id, grp", "id, grp, v", "id"])));
    if rng.chance(0.5) {
        out.push("GRANT SELECT ON r7k TO r7_bob".into());
    }
    if rng.chance(0.6) {
        for t in ["r7k", "r7p", "r7r", "r7g", "r7d", "r7t", "r7acct"] {
            out.push(format!("ANALYZE {t}"));
        }
    }
    out
}

/// Templates whose instances differ only in their literals (`{c}`: a `slot` of kind c), and
/// whether their rows come in a fixed order; DML names its table.
const TEMPLATES: &[(&str, bool, Option<&str>)] = &[
    ("SELECT id, v, note FROM r7k WHERE id = {k}", false, None),
    ("SELECT id, v FROM r7k WHERE id = {x} ORDER BY id", true, None),
    ("SELECT count(*), sum(v) FROM r7k WHERE grp = {g}", false, None),
    ("EXPLAIN (COSTS OFF) SELECT id FROM r7k WHERE grp = {g}", true, None),
    ("SELECT id, note FROM r7k WHERE note LIKE {p} ORDER BY id", true, None),
    ("EXPLAIN (COSTS OFF) SELECT id FROM r7k WHERE note LIKE {p}", true, None),
    ("SELECT id, v + {i} AS w FROM r7k WHERE id <= {k} ORDER BY id", true, None),
    ("SELECT {x} AS c, id FROM r7k WHERE id = 1", false, None),
    ("SELECT id, grp FROM r7k ORDER BY {s}, id LIMIT {s}", true, None),
    ("SELECT grp, count(*) FROM r7k GROUP BY {s} ORDER BY 1", true, None),
    ("SELECT id FROM r7k WHERE v BETWEEN {i} AND {i} ORDER BY id", true, None),
    ("SELECT id FROM r7k WHERE id IN ({k}, {k}, {x}) ORDER BY id", true, None),
    ("SELECT id, amt * {n} FROM r7k WHERE amt > {n} ORDER BY id", true, None),
    ("SELECT id, ts < {d}::date + {s} FROM r7k ORDER BY id", true, None),
    ("SELECT id, ts AT TIME ZONE {z}, date_trunc('day', ts, {z}) FROM r7k WHERE id <= {k} ORDER BY id", true, None),
    ("SELECT {t} || note, length({t}), upper({t}) FROM r7k WHERE id = {k}", false, None),
    ("SELECT CASE WHEN v > {i} THEN 'hi' ELSE {t} END FROM r7k WHERE id = {k}", false, None),
    ("SELECT id FROM r7k WHERE (grp, id) > ({g}, {k}) ORDER BY grp, id LIMIT 3", true, None),
    ("SELECT id, v FROM r7k WHERE id = ANY ({a}::int[]) ORDER BY id", true, None),
    ("VALUES ({x}), ({x})", false, None),
    ("SELECT {i} + {i}, {i}::int2, {x} * 2", false, None),
    ("SELECT round({n}, {s}), trunc({n}), {n} % 7, {n}::float8", false, None),
    ("SELECT id, region, qty FROM r7p WHERE region = {r} AND qty > {i} ORDER BY 1, 2", true, None),
    ("EXPLAIN (COSTS OFF) SELECT * FROM r7p WHERE region = {r}", true, None),
    ("SELECT s FROM r7t WHERE c = {t} ORDER BY id", true, None),
    ("SELECT {t}::r7code", false, None),
    ("SELECT {a}::r7pos[]", false, None),
    ("SELECT {w} + {v}, {w} AT TIME ZONE {z}, ({w} + {v})::date", false, None),
    ("SELECT id FROM r7k WHERE ts >= {w} - {v} ORDER BY id OFFSET {s}", true, None),
    ("UPDATE r7k SET v = {i} WHERE id = {k} RETURNING id, v", false, Some("r7k")),
    ("INSERT INTO r7k (id, grp, v, note) VALUES ({k} + 100, {g}, {i}, {t}) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v RETURNING id, v, note", false, Some("r7k")),
    ("UPDATE r7p SET region = {r} WHERE id = {k} RETURNING tableoid::regclass, id, region", false, Some("r7p")),
    ("UPDATE r7r SET qty = qty + {i} WHERE id = {k} RETURNING tableoid::regclass, id, qty", false, Some("r7r")),
    ("INSERT INTO r7d (id, p, ps, code) VALUES ({k} + 10, {i}, {a}, {t}) RETURNING *", false, Some("r7d")),
    ("DELETE FROM r7k WHERE id = {k} AND grp = {g} RETURNING id", false, Some("r7k")),
];

/// A template's instance: its SQL with literals, the same with `$n`, and the parameters.
fn instance(rng: &mut Rng, tpl: &str) -> (String, String, Vec<Lit>) {
    let (mut sql, mut psql, mut params) = (String::new(), String::new(), Vec::new());
    let mut it = tpl.chars().peekable();
    while let Some(c) = it.next() {
        if c == '{' && it.peek().is_some_and(|c| c.is_ascii_lowercase()) {
            let k = it.next().unwrap();
            it.next();
            let l = slot(rng, k);
            sql.push_str(&l.sql);
            params.push(l);
            psql.push_str(&format!("${}", params.len()));
        } else {
            sql.push(c);
            psql.push(c);
        }
    }
    (sql, psql, params)
}

/// An extended-protocol step on session `s`: parameters typed or not, text or binary.
fn ext_step(rng: &mut Rng, s: u8, name: &str, sql: Option<&str>, params: &[Lit], describe: bool) -> String {
    let typed = rng.chance(0.6);
    let mut head = format!("\\x{s}");
    if !name.is_empty() {
        head.push_str(&format!(" name={name}"));
    }
    if typed && sql.is_some() {
        let t: Vec<String> = params.iter().map(|p| p.oid.to_string()).collect();
        head.push_str(&format!(" types={}", t.join(",")));
    }
    if describe {
        head.push_str(" describe");
    }
    match rng.below(8) {
        0..=2 => {}
        3 | 4 => head.push_str(" rf=1"),
        5 => head.push_str(" rf=0"),
        6 => head.push_str(" rf=1,0"),
        _ => head.push_str(&format!(" rows={}", rng.range(1, 2))),
    }
    for p in params {
        let bin = p.bin.as_ref().filter(|_| (typed || rng.chance(0.15)) && rng.chance(0.5));
        match (&p.text, bin) {
            (None, _) => head.push_str(" p=null"),
            (Some(_), Some(b)) => head.push_str(&format!(" p=b{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())),
            (Some(t), None) => head.push_str(&format!(" p=t'{}'", t.replace('\'', "''"))),
        }
    }
    format!("{head}\n{}", sql.unwrap_or("-"))
}

/// The session a statement goes to: 1 (in-process), 2 or 3 (simple protocol over the wire).
fn on(rng: &mut Rng, sql: String) -> String {
    match rng.below(4) {
        0 | 1 => sql,
        2 => format!("\\s2 {sql}"),
        _ => format!("\\s3 {sql}"),
    }
}

fn template_stmt(sql: String, ordered: bool, table: Option<&str>) -> Stmt {
    match table {
        Some(t) => dml(sql, t),
        None => query(sql, ordered),
    }
}

/// One template, several times with other literals, from every session and both protocols;
/// a named statement executed past the five custom plans.
fn literal_variants(rng: &mut Rng, serial: u64) -> Vec<Stmt> {
    let (tpl, ordered, table) = *rng.pick(TEMPLATES);
    let mut out = Vec::new();
    for _ in 0..rng.range(2, 4) {
        let (sql, psql, params) = instance(rng, tpl);
        out.push(template_stmt(on(rng, sql), ordered, table));
        if rng.chance(0.5) {
            let s = 2 + rng.below(2) as u8;
            let describe = rng.chance(0.3);
            out.push(template_stmt(ext_step(rng, s, "", Some(&psql), &params, describe), ordered, table));
        }
    }
    if rng.chance(0.35) {
        let s = 2 + rng.below(2) as u8;
        let name = format!("r7n{serial}");
        let (_, psql, params) = instance(rng, tpl);
        out.push(template_stmt(ext_step(rng, s, &name, Some(&psql), &params, true), ordered, table));
        for _ in 0..rng.range(5, 8) {
            let (_, _, params) = instance(rng, tpl);
            out.push(template_stmt(ext_step(rng, s, &name, None, &params, false), ordered, table));
        }
        out.push(o(format!("\\s{s} DEALLOCATE {name}")));
    }
    out
}

/// Statements that fail (each its own way), and ones that change rows.
const BAD: &[&str] = &[
    "INSERT INTO r7k (id, grp) VALUES (1, 1)",
    "UPDATE r7d SET p = 0 WHERE id = 1",
    "SELECT 1 / 0",
    "INSERT INTO r7g (id, a) VALUES (1, 1)",
    "UPDATE r7g SET b = 1",
    "INSERT INTO r7r VALUES (9, NULL, 'x')",
    "SELECT * FROM r7nosuch",
    "INSERT INTO r7d (id, code) VALUES (50, 'abc')",
    "SELECT 'x'::int",
    "UPDATE r7g SET a = a * 2 RETURNING b",
    "INSERT INTO r7p VALUES (1, 'eu', 0)",
];
const GOOD: &[&str] = &[
    "UPDATE r7k SET v = coalesce(v, 0) + 1 WHERE id = 2",
    "INSERT INTO r7k (id, grp, v) VALUES (300, 2, 0) ON CONFLICT (id) DO UPDATE SET v = r7k.v + 1",
    "DELETE FROM r7p WHERE id = 1",
    "INSERT INTO r7g (a, t) VALUES (5, 'sv')",
    "UPDATE r7r SET qty = qty + 60 WHERE id = 3",
    "UPDATE r7d SET ps = array_append(ps, 9) WHERE id = 1",
];

/// A statement on session `s`.
fn at(s: u8, sql: &str) -> Stmt {
    if s == 1 { o(sql) } else { o(format!("\\s{s} {sql}")) }
}

/// Every table's rows, in key order, as session 1 sees them.
fn state(rng: &mut Rng) -> Stmt {
    let q = *rng.pick(&[
        "SELECT id, grp, v FROM r7k ORDER BY id",
        "SELECT tableoid::regclass, * FROM r7p ORDER BY id, region",
        "SELECT tableoid::regclass, * FROM r7r ORDER BY id, qty",
        "SELECT * FROM r7g ORDER BY id",
        "SELECT * FROM r7d ORDER BY id",
    ]);
    query(q.into(), true)
}

/// A transaction with savepoints and errors on one session, the state after each.
fn savepoints(rng: &mut Rng) -> Vec<Stmt> {
    let s = 1 + rng.below(3) as u8;
    let mut out = vec![at(s, rng.pick(&["BEGIN", "BEGIN", "BEGIN ISOLATION LEVEL REPEATABLE READ", "START TRANSACTION READ WRITE"]))];
    out.push(at(s, rng.pick(GOOD)));
    out.push(at(s, "SAVEPOINT a"));
    out.push(at(s, rng.pick(GOOD)));
    match rng.below(6) {
        0 => {
            out.push(at(s, "SAVEPOINT b"));
            out.push(at(s, rng.pick(BAD)));
            out.push(at(s, "SELECT 1"));
            out.push(at(s, "ROLLBACK TO SAVEPOINT b"));
            out.push(at(s, "RELEASE SAVEPOINT b"));
        }
        1 => {
            out.push(at(s, rng.pick(BAD)));
            out.push(at(s, "RELEASE a"));
            out.push(at(s, "ROLLBACK TO a"));
        }
        2 => {
            out.push(at(s, "ROLLBACK TO SAVEPOINT nosuch"));
            out.push(at(s, "ROLLBACK TO a"));
        }
        3 if s > 1 => {
            // An error over the extended protocol inside the transaction.
            out.push(o(format!("\\x{s} p=t'0'\nSELECT 1 / $1::int")));
            out.push(at(s, "SELECT 1"));
            out.push(at(s, "ROLLBACK TO a"));
        }
        4 => {
            out.push(at(s, "SAVEPOINT a"));
            out.push(at(s, rng.pick(GOOD)));
            out.push(at(s, "ROLLBACK TO a"));
            out.push(at(s, "RELEASE a"));
            out.push(at(s, "ROLLBACK TO a"));
        }
        _ => {
            out.push(at(s, rng.pick(BAD)));
            out.push(at(s, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"));
        }
    }
    out.push(at(s, rng.pick(GOOD)));
    out.push(at(s, rng.pick(&["COMMIT", "COMMIT", "ROLLBACK", "COMMIT AND CHAIN", "END"])));
    // (After COMMIT AND CHAIN: still in a transaction.)
    out.push(at(s, "ROLLBACK"));
    if rng.chance(0.3) {
        out.push(at(s, rng.pick(&["SAVEPOINT x", "RELEASE x", "ROLLBACK TO x", "COMMIT", "SET TRANSACTION READ ONLY"])));
    }
    out.push(state(rng));
    out
}

/// Two sessions interleaved so that each step's outcome is fixed: a statement that must wait
/// is sent with `&` and collected with `.` after the other session ends its transaction.
fn concurrent(rng: &mut Rng) -> Vec<Stmt> {
    let k = rng.range(1, 8);
    let end = *rng.pick(&["COMMIT", "ROLLBACK"]);
    let mut out = Vec::new();
    let mut p = |s: &str| out.push(o(s.to_string()));
    match rng.below(9) {
        0 | 1 => {
            // A row lock waited for, the row re-checked after the other session's change.
            let iso = *rng.pick(&["", "", " ISOLATION LEVEL REPEATABLE READ"]);
            p("\\s2 BEGIN");
            p(&format!("\\s2 UPDATE r7k SET v = coalesce(v, 0) + 10, grp = grp + {} WHERE id = {k}", rng.below(2)));
            p(&format!("\\s3 BEGIN{iso}"));
            if !iso.is_empty() {
                p("\\s3 SELECT count(*) FROM r7k");
            }
            p(&format!("\\s3& {}", rng.pick(&[
                format!("UPDATE r7k SET v = v * 2 WHERE id = {k} AND grp = 1 RETURNING id, v, grp"),
                format!("DELETE FROM r7k WHERE id = {k} AND grp = 1 RETURNING id"),
                format!("SELECT id, v, grp FROM r7k WHERE id = {k} AND grp = 1 FOR UPDATE"),
                format!("SELECT id, v FROM r7k WHERE id = {k} FOR NO KEY UPDATE"),
                format!("SELECT id, v FROM r7k WHERE id = {k} FOR SHARE"),
                format!("SELECT id, v FROM r7k WHERE id = {k} FOR KEY SHARE"),
                format!("UPDATE r7k SET v = 0 WHERE grp = 1 AND id <= {k} RETURNING id"),
            ])));
            p(&format!("\\s2 {end}"));
            p("\\s3.");
            p("\\s3 COMMIT");
        }
        2 => {
            // SKIP LOCKED and NOWAIT against row locks of each strength.
            let held = *rng.pick(&["UPDATE", "NO KEY UPDATE", "SHARE", "KEY SHARE"]);
            p("\\s2 BEGIN");
            p(&format!("\\s2 SELECT id FROM r7k WHERE grp = 1 ORDER BY id LIMIT 2 FOR {held}"));
            let want = *rng.pick(&["UPDATE", "NO KEY UPDATE", "SHARE", "KEY SHARE"]);
            p(&format!("\\s3 SELECT id FROM r7k ORDER BY id LIMIT 3 FOR {want} SKIP LOCKED"));
            p(&format!("\\s3 SELECT id FROM r7k WHERE id = {k} FOR {want} NOWAIT"));
            p("\\s3 UPDATE r7k SET v = 0 WHERE id IN (SELECT id FROM r7k ORDER BY id LIMIT 2 FOR UPDATE SKIP LOCKED) RETURNING id");
            p("\\s3 SET lock_timeout = '20ms'");
            p(&format!("\\s3 SELECT id FROM r7k WHERE grp = 1 ORDER BY id LIMIT 1 FOR {want}"));
            p("\\s3 RESET lock_timeout");
            p(&format!("\\s2 {end}"));
        }
        3 => {
            // A row moved to another partition while the other session waited for it.
            let (t, change) = *rng.pick(&[
                ("r7p", "region = CASE region WHEN 'eu' THEN 'us' ELSE 'eu' END"),
                ("r7r", "qty = qty + 100"),
                ("r7p", "qty = coalesce(qty, 0) + 1"),
            ]);
            let k = rng.range(1, 3);
            p("\\s2 BEGIN");
            p(&format!("\\s2 UPDATE {t} SET {change} WHERE id = {k}"));
            p(&format!("\\s3& {}", rng.pick(&[
                format!("UPDATE {t} SET qty = qty + 1 WHERE id = {k} RETURNING tableoid::regclass, qty"),
                format!("DELETE FROM {t} WHERE id = {k} RETURNING id"),
                format!("SELECT id, qty FROM {t} WHERE id = {k} FOR UPDATE"),
            ])));
            p(&format!("\\s2 {end}"));
            p("\\s3.");
        }
        4 => {
            // A unique key inserted by both sessions.
            let id = 500 + k;
            p("\\s2 BEGIN");
            p(&format!("\\s2 INSERT INTO r7k (id, grp, v) VALUES ({id}, 1, 1)"));
            let tail = *rng.pick(&["", " ON CONFLICT (id) DO NOTHING", " ON CONFLICT (id) DO UPDATE SET v = r7k.v + 100"]);
            p(&format!("\\s3& INSERT INTO r7k (id, grp, v) VALUES ({id}, 2, 2){tail} RETURNING id, grp, v"));
            p(&format!("\\s2 {end}"));
            p("\\s3.");
        }
        5 => {
            // A deadlock: session 3 (the short deadlock_timeout) finds it and fails.
            let b = k % 8 + 1;
            p("\\s2 SET deadlock_timeout = '20s'");
            p("\\s3 SET deadlock_timeout = '100ms'");
            p("\\s2 BEGIN");
            p("\\s3 BEGIN");
            p(&format!("\\s2 UPDATE r7k SET v = 1 WHERE id = {k}"));
            p(&format!("\\s3 UPDATE r7k SET v = 2 WHERE id = {b}"));
            p(&format!("\\s2& UPDATE r7k SET v = 3 WHERE id = {b}"));
            p(&format!("\\s3 UPDATE r7k SET v = 4 WHERE id = {k}"));
            p("\\s3 ROLLBACK");
            p("\\s2.");
            p(&format!("\\s2 {end}"));
            p("\\s2 RESET deadlock_timeout");
            p("\\s3 RESET deadlock_timeout");
        }
        6 => {
            // Table locks: NOWAIT, then a write waiting for the lock.
            let held = *rng.pick(&["SHARE", "SHARE ROW EXCLUSIVE", "EXCLUSIVE", "ACCESS SHARE", "ROW EXCLUSIVE", "SHARE UPDATE EXCLUSIVE"]);
            let want = *rng.pick(&["ROW SHARE", "ROW EXCLUSIVE", "SHARE", "EXCLUSIVE", "SHARE UPDATE EXCLUSIVE"]);
            p("\\s2 BEGIN");
            p(&format!("\\s2 LOCK TABLE r7k IN {held} MODE"));
            p("\\s3 BEGIN");
            p(&format!("\\s3 LOCK TABLE r7k IN {want} MODE NOWAIT"));
            p("\\s3 ROLLBACK");
            p(&format!("\\s3& UPDATE r7k SET v = v WHERE id = {k} RETURNING id"));
            p(&format!("\\s2 {end}"));
            p("\\s3.");
        }
        7 => {
            // Advisory locks, session and transaction level.
            p(&format!("\\s2 SELECT pg_advisory_lock({k})"));
            p(&format!("\\s3 SELECT pg_try_advisory_lock({k}), pg_try_advisory_lock_shared({k}), pg_try_advisory_xact_lock({k})"));
            p(&format!("\\s3& SELECT pg_advisory_lock({k})"));
            p(&format!("\\s2 SELECT pg_advisory_unlock({k}), pg_advisory_unlock({k})"));
            p("\\s3.");
            p("\\s3 SELECT locktype, mode, granted FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid()");
            p("\\s3 SELECT pg_advisory_unlock_all()");
            p("\\s2 SELECT pg_advisory_unlock_all()");
        }
        _ => {
            // Serializable write skew: both read both groups, each writes the other's.
            p("\\s2 BEGIN ISOLATION LEVEL SERIALIZABLE");
            p("\\s3 BEGIN ISOLATION LEVEL SERIALIZABLE");
            p("\\s2 SELECT sum(v) FROM r7k WHERE grp IN (1, 2)");
            p("\\s3 SELECT sum(v) FROM r7k WHERE grp IN (1, 2)");
            p("\\s2 UPDATE r7k SET v = coalesce(v, 0) + 1 WHERE id = (SELECT min(id) FROM r7k WHERE grp = 2)");
            p("\\s3 UPDATE r7k SET v = coalesce(v, 0) + 1 WHERE id = (SELECT min(id) FROM r7k WHERE grp = 1)");
            p("\\s2 COMMIT");
            p("\\s3 COMMIT");
        }
    }
    // (Whatever happened, both sessions end outside a transaction.)
    p("\\s2 ROLLBACK");
    p("\\s3 ROLLBACK");
    out.push(state(rng));
    out
}

/// MERGE with every kind of action, into plain, partitioned, generated-column and domain
/// tables, from VALUES (keys repeated sometimes) or a table.
fn merge(rng: &mut Rng) -> Vec<Stmt> {
    let (t, key, src_cols, set, ins) = *rng.pick(&[
        ("r7k", "id", "id, v", "v = s.v", "(id, grp, v) VALUES (s.id, 9, s.v)"),
        ("r7p", "id", "id, v", "region = CASE WHEN s.v > 50 THEN 'us' ELSE 'eu' END, qty = s.v", "VALUES (s.id, CASE WHEN s.v < 0 THEN 'xx' ELSE 'eu' END, s.v)"),
        ("r7r", "id", "id, v", "qty = s.v", "VALUES (s.id, s.v, 'm')"),
        ("r7g", "a", "id, v", "a = s.v, t = 'm' || s.id", "(a, t) VALUES (s.v, 'n')"),
        ("r7g", "id", "id, v", "a = s.v", "(id, a) OVERRIDING SYSTEM VALUE VALUES (s.id + 1000, s.v)"),
        ("r7d", "id", "id, v", "p = s.v, ps = ARRAY[s.v]", "(id, p, code) VALUES (s.id, s.v, 'QQ' || (s.id % 10))"),
    ]);
    let mut rows = Vec::new();
    for _ in 0..rng.range(1, 4) {
        let id = rng.range(0, 12);
        let v = *rng.pick(&[-5, 0, 7, 60, 150, 2147483647]);
        rows.push(format!("({id}, {v})"));
    }
    if rng.chance(0.15) {
        rows.push(rows[0].clone());
    }
    let src = if rng.chance(0.2) { "(SELECT id, qty AS v FROM r7r) s".to_string() } else { format!("(VALUES {}) s({src_cols})", rows.join(", ")) };
    let mut whens = Vec::new();
    if rng.chance(0.4) {
        whens.push("WHEN MATCHED AND s.v < 0 THEN DELETE".to_string());
    }
    if rng.chance(0.3) {
        whens.push("WHEN MATCHED AND s.v = 0 THEN DO NOTHING".into());
    }
    if rng.chance(0.8) {
        whens.push(format!("WHEN MATCHED THEN UPDATE SET {set}"));
    }
    if rng.chance(0.3) {
        whens.push(format!("WHEN NOT MATCHED {}AND s.v > 100 THEN DO NOTHING", if rng.chance(0.5) { "BY TARGET " } else { "" }));
    }
    if rng.chance(0.8) {
        whens.push(format!("WHEN NOT MATCHED THEN INSERT {ins}"));
    }
    if rng.chance(0.3) {
        whens.push(format!("WHEN NOT MATCHED BY SOURCE AND t.{key} > 8 THEN DELETE"));
    }
    if rng.chance(0.2) {
        whens.push(format!("WHEN NOT MATCHED BY SOURCE THEN UPDATE SET {}", set.replace("s.v", "1").replace("s.id", "0")));
    }
    if whens.is_empty() {
        whens.push("WHEN MATCHED THEN DO NOTHING".into());
    }
    let ret = *rng.pick(&["", " RETURNING merge_action(), t.*", " RETURNING merge_action(), old.*, new.*"]);
    let sql = format!("MERGE INTO {t} t USING {src} ON t.{key} = s.id {}{ret}", whens.join(" "));
    vec![dml(on(rng, sql), t)]
}

/// DML on partitioned tables (rows moving between partitions, the default one), generated
/// and identity columns, and domains with checks and arrays of them.
fn columns(rng: &mut Rng) -> Vec<Stmt> {
    let k = rng.range(1, 6);
    let qty = *rng.pick(&[-10, 50, 200, 100, 0]);
    let choices = [
        (format!("UPDATE r7r SET qty = {qty} WHERE id = {k} RETURNING tableoid::regclass, *"), "r7r"),
        (format!("INSERT INTO r7p VALUES ({k}, 'xx', 1) RETURNING tableoid::regclass, *"), "r7p"),
        (format!("UPDATE r7p SET region = 'xx' WHERE id = {k} RETURNING tableoid::regclass, *"), "r7p"),
        (format!("UPDATE r7p SET id = id + 20, region = 'us' WHERE id <= {k} RETURNING tableoid::regclass, *"), "r7p"),
        ("INSERT INTO r7g (a, t) VALUES (2, 'q') RETURNING *".into(), "r7g"),
        ("INSERT INTO r7g (id, a) OVERRIDING SYSTEM VALUE VALUES (1000, 1) RETURNING *".into(), "r7g"),
        ("INSERT INTO r7g (id, a) VALUES (1001, 1) RETURNING *".into(), "r7g"),
        ("INSERT INTO r7g (id, a) OVERRIDING USER VALUE VALUES (1002, 1) RETURNING *".into(), "r7g"),
        ("INSERT INTO r7g (a, b) VALUES (1, 5)".into(), "r7g"),
        ("INSERT INTO r7g (a, b, c) VALUES (1, DEFAULT, DEFAULT) RETURNING b, c".into(), "r7g"),
        ("UPDATE r7g SET a = a + 1 WHERE a IS NOT NULL RETURNING id, b, c".into(), "r7g"),
        ("UPDATE r7g SET t = NULL, a = -a RETURNING c".into(), "r7g"),
        ("UPDATE r7d SET ps = ps || 0 WHERE id = 1".into(), "r7d"),
        ("UPDATE r7d SET ps[2] = -1 WHERE id = 1".into(), "r7d"),
        ("UPDATE r7d SET ps[3] = 4 WHERE id = 1 RETURNING ps".into(), "r7d"),
        (format!("INSERT INTO r7d (id, m) VALUES ({k} + 20, NULL)"), "r7d"),
        ("INSERT INTO r7d (id, m) VALUES (40, -1000.005) RETURNING m".into(), "r7d"),
        ("UPDATE r7d SET code = lower(code)".into(), "r7d"),
        ("UPDATE r7d SET m = m * 1000000000 WHERE id = 3".into(), "r7d"),
        ("INSERT INTO r7d (id, ps) VALUES (41, '{1,NULL}') RETURNING ps".into(), "r7d"),
    ];
    let (sql, t) = rng.pick(&choices).clone();
    let mut out = vec![dml(on(rng, sql), t)];
    if rng.chance(0.25) {
        out.push(o(*rng.pick(&[
            "ALTER TABLE r7g ALTER COLUMN id RESTART WITH 7",
            "ALTER TABLE r7g ALTER COLUMN id SET INCREMENT BY 1",
            "ALTER DOMAIN r7pos ADD CONSTRAINT r7lt CHECK (VALUE < 1000) NOT VALID",
            "ALTER DOMAIN r7pos VALIDATE CONSTRAINT r7lt",
            "ALTER DOMAIN r7code DROP CONSTRAINT r7code_check",
            "ALTER TABLE r7g ALTER COLUMN c SET EXPRESSION AS (t || '!')",
            "ALTER TABLE r7p DETACH PARTITION r7p_us",
            "CREATE TABLE r7p_ap PARTITION OF r7p FOR VALUES IN ('ap')",
        ])));
        out.push(state(rng));
    }
    out
}

/// Collations (ICU, nondeterministic), time zones around DST changes, numeric and float edges.
fn edges(rng: &mut Rng) -> Vec<Stmt> {
    let qs: &[&str] = match rng.below(3) {
        0 => &[
            "SELECT id, s FROM r7t ORDER BY s COLLATE r7de, id",
            "SELECT id, s FROM r7t ORDER BY s COLLATE \"und-x-icu\" DESC, id",
            "SELECT id, s FROM r7t ORDER BY c, id",
            "SELECT count(DISTINCT c), count(DISTINCT s) FROM r7t",
            "SELECT id FROM r7t WHERE c = 'A' ORDER BY id",
            "SELECT id FROM r7t WHERE c LIKE 'a%' ORDER BY id",
            "SELECT 'ß' = 'ss' COLLATE r7ci, 'Æ' = 'ae' COLLATE r7ci, 'a' < 'B' COLLATE r7de",
            "SELECT upper(s COLLATE \"de-x-icu\"), lower(s COLLATE \"tr-x-icu\") FROM r7t ORDER BY id",
            "SELECT string_agg(s, ',' ORDER BY s COLLATE r7de, id) FROM r7t",
            "SELECT id FROM r7t WHERE s > 'a' COLLATE \"C\" ORDER BY id",
            // (Which of equal values a UNION keeps depends on the plan: only how many.)
            "SELECT count(*) FROM (SELECT s COLLATE r7ci FROM r7t UNION SELECT 'A') u",
            "SELECT id FROM r7t WHERE c IN ('a', 'E') ORDER BY id",
        ],
        1 => &[
            "SELECT '2025-03-09 02:30'::timestamptz, '2025-11-02 01:30'::timestamptz",
            "SELECT '2025-03-09 01:59:59'::timestamptz + interval '1 second', '2025-03-08 12:00'::timestamptz + interval '1 day', '2025-03-08 12:00'::timestamptz + interval '24 hours'",
            "SELECT date_trunc('day', '2025-11-02 12:00'::timestamptz), date_trunc('hour', '2025-11-02 01:30-05'::timestamptz)",
            "SELECT generate_series('2025-03-08'::timestamptz, '2025-03-10', '12 hours')",
            "SELECT to_char('2025-11-02 01:30-05'::timestamptz, 'YYYY-MM-DD HH24:MI TZ OF')",
            "SELECT extract(epoch FROM '2025-03-09 03:00'::timestamptz - '2025-03-09 01:00'::timestamptz), age('2025-03-10'::timestamptz, '2025-03-08'::timestamptz)",
            "SELECT '2025-03-09 02:30'::timestamp AT TIME ZONE 'America/New_York', timezone('Australia/Lord_Howe', '2025-04-06 01:45'::timestamp)",
            "SELECT date_bin('1 hour', '2025-11-02 01:30-04'::timestamptz, '2000-01-01'), make_timestamptz(2025, 3, 9, 2, 30, 0, 'America/New_York')",
            "SELECT '2025-03-30 02:30 Europe/Berlin'::timestamptz, '2025-10-26 02:30 Europe/Berlin'::timestamptz",
            "SELECT id, ts, ts::date, ts::time, extract(hour FROM ts) FROM r7k ORDER BY id",
            "SELECT generate_series('2025-11-01'::timestamptz, '2025-11-03', '1 day', 'America/New_York')",
            "SELECT '2025-03-09'::date + '02:30'::time, ('2025-03-09'::date + '02:30'::time)::timestamptz",
        ],
        _ => &[
            "SELECT 'NaN'::numeric = 'NaN', 'NaN'::float8 > 'Infinity', 'Infinity'::numeric - 'Infinity'",
            "SELECT '-0'::float8, '-0'::float8 = 0, 1 / '-0'::float8",
            "SELECT 2.5::float8::int, 3.5::float8::int, (-2.5)::float8::int, 2.5::numeric::int, (-2.5)::numeric::int",
            "SELECT 1e308::float8 * 10",
            "SELECT 1e-320::float8, '1.17549435e-38'::float4, '3.4028235e38'::float4",
            "SELECT '1e39'::float4",
            "SELECT 0.1::float4 + 0.2::float4, 0.1::float8 + 0.2, 1e15::float8, 1e16::float8, 123456789012345678::float8",
            "SELECT 123.456::numeric(5, 2)",
            "SELECT round(2.5), round(-2.5), round(2.5::float8), round(-0.5::float8), round(1234.5678, -2)",
            "SELECT 10::numeric / 3, 1::numeric / 7 * 7, -7 / 2, -7 % 2, mod(-7.5, 2), div(-7.5, 2)",
            "SELECT power(2::numeric, 100), power(-8, 1.0 / 3)",
            "SELECT sqrt(-1)",
            "SELECT ln(0)",
            "SELECT exp(1000)",
            "SELECT 2147483647::int4 + 1",
            "SELECT (-2147483648)::int4 / -1",
            "SELECT 'Infinity'::numeric::float8, 'NaN'::float8::numeric, 1e300::float8::numeric::text",
            "SELECT sum(f), max(f), min(f), count(f) FROM r7k",
            "SELECT id, f::numeric FROM r7k ORDER BY id",
            "SELECT var_samp(amt), stddev(amt), sum(amt), avg(amt) FROM r7k",
            "SELECT to_char(1234.5678::numeric, '9G999D99'), to_char(-0.5, 'S9D9'), to_number('1,234.5', '9G999D9')",
            "SELECT 1.0e-130::numeric, 0.000001::float8, 1e-7::float8",
            "SELECT width_bucket(5.5, 0, 10, 3), width_bucket('NaN'::numeric, 0, 10, 3)",
            "SELECT trim_scale(1.500), scale(1.500), min_scale(1.500), gcd(-12, 18), lcm(4::numeric, 6.0)",
        ],
    };
    let tz = rng.below(3) == 1;
    let mut out = Vec::new();
    if tz {
        out.push(o(format!("SET TimeZone = '{}'", rng.pick(&["America/New_York", "Europe/Berlin", "Australia/Lord_Howe", "Asia/Kathmandu"]))));
    }
    if rng.chance(0.15) {
        out.push(o("SET extra_float_digits = 0"));
    }
    for _ in 0..rng.range(2, 4) {
        let q = rng.pick(qs).to_string();
        out.push(query(on(rng, q), true));
    }
    out.push(o("SET TimeZone = 'UTC'"));
    out.push(o("SET extra_float_digits = 1"));
    out
}

/// GRANT / REVOKE and row security: statements as one role or another on one session, the
/// policies and privileges changed between them.
fn roles(rng: &mut Rng) -> Vec<Stmt> {
    let s = 1 + rng.below(3) as u8;
    let k = rng.range(6, 12);
    let mut out = Vec::new();
    if rng.chance(0.4) {
        out.push(o(*rng.pick(&[
            "REVOKE SELECT ON r7acct FROM r7_bob",
            "GRANT UPDATE (amount) ON r7acct TO r7_bob",
            "GRANT SELECT ON r7k TO PUBLIC",
            "REVOKE ALL ON r7k FROM r7_alice",
            "GRANT DELETE ON r7acct TO r7_alice",
            "ALTER TABLE r7acct DISABLE ROW LEVEL SECURITY",
            "ALTER TABLE r7acct ENABLE ROW LEVEL SECURITY",
            "ALTER POLICY r7_own ON r7acct USING (owner = current_user OR region = 'us')",
            "DROP POLICY IF EXISTS r7_region ON r7acct",
            "CREATE POLICY r7_upd ON r7acct FOR UPDATE USING (amount < 100) WITH CHECK (amount < 1000)",
            // (No role memberships: they are the cluster's, and runs share the reference.)
            "ALTER TABLE r7acct OWNER TO r7_bob",
        ])));
    }
    let role = *rng.pick(&["r7_alice", "r7_bob"]);
    out.push(at(s, &format!("SET ROLE {role}")));
    let ops = [
        "SELECT id, owner, amount FROM r7acct ORDER BY id".to_string(),
        format!("INSERT INTO r7acct (id, amount, region) VALUES ({k}, -1, 'eu')"),
        format!("INSERT INTO r7acct (id, amount, region) VALUES ({k}, 5, 'us') RETURNING owner"),
        format!("INSERT INTO r7acct (id, owner, amount) VALUES ({k}, 'r7_bob', 5)"),
        "UPDATE r7acct SET amount = amount + 1 RETURNING id, amount".into(),
        "UPDATE r7acct SET owner = 'x' WHERE id = 1".into(),
        "UPDATE r7acct SET region = 'xx' WHERE id = 1 RETURNING id".into(),
        "DELETE FROM r7acct WHERE id = 2 RETURNING id".into(),
        "SELECT id, grp FROM r7k ORDER BY id LIMIT 3".into(),
        "SELECT id, v FROM r7k ORDER BY id LIMIT 3".into(),
        "SELECT count(*) FROM r7k".into(),
        "SELECT current_user, session_user, current_role".into(),
        "SELECT * FROM r7acct WHERE amount > 0 FOR UPDATE".into(),
        format!("INSERT INTO r7acct (id, amount) VALUES ({k}, 1) ON CONFLICT (id) DO UPDATE SET amount = 2 RETURNING id"),
        "MERGE INTO r7acct t USING (VALUES (1, 3), (2, 4)) s(id, a) ON t.id = s.id WHEN MATCHED THEN UPDATE SET amount = s.a".into(),
        "COPY r7acct TO STDOUT".into(),
        "SELECT has_table_privilege('r7acct', 'DELETE'), has_column_privilege('r7k', 'v', 'SELECT'), row_security_active('r7acct')".into(),
    ];
    for _ in 0..rng.range(2, 4) {
        out.push(at(s, rng.pick(&ops)));
    }
    out.push(at(s, "RESET ROLE"));
    out.push(query(
        rng.pick(&[
            "SELECT * FROM r7acct ORDER BY id",
            "SELECT grantee, privilege_type, is_grantable FROM information_schema.role_table_grants WHERE table_name = 'r7acct' ORDER BY 1, 2",
            "SELECT polname, polcmd, polpermissive, pg_get_expr(polqual, polrelid), pg_get_expr(polwithcheck, polrelid) FROM pg_policy WHERE polrelid = 'r7acct'::regclass ORDER BY 1",
            "SELECT relrowsecurity, relforcerowsecurity, relacl FROM pg_class WHERE relname IN ('r7acct', 'r7k') ORDER BY relname",
            "SELECT attname, attacl FROM pg_attribute WHERE attrelid = 'r7k'::regclass AND attacl IS NOT NULL ORDER BY 1",
            "SELECT has_table_privilege('r7_alice', 'r7acct', 'DELETE'), has_column_privilege('r7_alice', 'r7k', 'v', 'SELECT'), pg_has_role('r7_alice', 'r7_bob', 'MEMBER')",
        ])
        .to_string(),
        true,
    ));
    out
}

impl Gen<'_> {
    /// Round 7's statements, weighted toward templates that differ only in their literals.
    pub(super) fn round7_stmt(&mut self) -> Vec<Stmt> {
        let rng = &mut self.rng;
        match rng.below(100) {
            0..=34 => {
                let serial = rng.next() % 100_000;
                literal_variants(rng, serial)
            }
            35..=46 => savepoints(rng),
            47..=63 => concurrent(rng),
            64..=72 => merge(rng),
            73..=81 => columns(rng),
            82..=91 => edges(rng),
            _ => roles(rng),
        }
    }
}
