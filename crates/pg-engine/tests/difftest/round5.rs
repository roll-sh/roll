//! Round 5 of the grammar (seeds from `R5_SEEDS` on): DDL interleaved with DML (ALTER TABLE
//! type changes, defaults, stored and virtual generated columns, constraints NOT VALID then
//! VALIDATE, partition attach / detach), triggers (row / statement, BEFORE / AFTER / INSTEAD OF,
//! transition tables), rules and updatable views WITH CHECK OPTION, savepoints and rollbacks
//! with cursors open, prepared statements with parameters of every type, COPY in and out,
//! ICU and libc collations in ORDER BY / GROUP BY / indexes, full text search, range and
//! multirange operators, arrays of composites, large numerics and intervals.
//!
//! Its objects are all named `r5*`; DDL and DML statements are followed by the table's
//! contents (`Stmt::Dml`) and, after DDL, its catalog description, so the final state is
//! compared and not just the statements' output.

use super::{Query, Rng, Stmt, Ty};

/// Texts for collation-sensitive columns: case and accent variants, digits, ß / ss.
const TEXTS5: &[&str] = &[
    "a", "A", "b", "B", "ab", "Ab", "AB", "ä", "Ä", "ae", "ß", "ss", "SS", "é", "e", "E", "10", "9", "2", "z", "Z",
    "å", "ö", "o", "ch", "c", "h", "-a", "a-", " a", "", "ı", "I", "i", "İ",
];
/// Collations of `r5a.b` and the ones ORDER BY / GROUP BY / indexes pick from.
pub const COLLS5: &[&str] = &[
    "C", "POSIX", "ucs_basic", "pg_c_utf8", "unicode", "und-x-icu", "en-x-icu", "sv-x-icu", "tr-x-icu", "de-x-icu",
    "r5ci", "r5num", "r5ai",
];
const DOCS: &[&str] = &[
    "The quick brown fox jumps over the lazy dog",
    "a fat cat sat on a mat and ate a fat rat",
    "running runners ran; the runner runs",
    "Supernovae stars are the brightest phenomena in galaxies",
    "PostgreSQL full text search: lexemes & weights",
    "the and of",
    "fox-hound e-mail user@example.com http://x.org/p?q=1 3.14 -42",
    "Fat cats! FAT rats? fat-free",
    "",
];
pub const NUMS5: &[&str] = &[
    "1e100", "-99999999999999999999999999999999999.999999", "0.0000000000000000000000000001", "NaN", "Infinity",
    "-Infinity", "123456789012345678901234567890.123456789", "0", "-0.5", "2.5", "1e-130", "7", "-1e40",
];
pub const IVS5: &[&str] = &[
    "178000000 years", "-178000000 years", "2147483647 days", "-2147483648 days", "2147483647 mons", "infinity",
    "-infinity", "1 day 00:00:00.000001", "1 mon", "30 days", "720:00:00", "-1 mon 30 days", "0", "1 year -1 sec",
];
const RANGES: &[&str] = &["[1,5)", "(1,5]", "empty", "[3,)", "(,2]", "(,)", "[5,6)", "[0,10)", "[-3,0]"];
const MRANGES: &[&str] = &["{}", "{[1,3),[5,7)}", "{[2,4)}", "{(,0],[10,)}", "{[1,2],[2,3)}", "{[0,100)}", "{empty}"];

fn q(rng: &mut Rng, v: &[&str], nulls: f64) -> String {
    if rng.chance(nulls) {
        return "NULL".into();
    }
    format!("'{}'", rng.pick(v).replace('\'', "''"))
}

/// A `r5ct` value, or an array of them.
fn ct(rng: &mut Rng) -> String {
    let x = if rng.chance(0.15) { "NULL".to_string() } else { rng.range(-2, 5).to_string() };
    format!("ROW({x}, {})::r5ct", q(rng, TEXTS5, 0.15))
}
fn cts(rng: &mut Rng) -> String {
    match rng.below(5) {
        0 => "NULL".into(),
        1 => "'{}'".into(),
        _ => {
            let v: Vec<String> = (0..rng.range(1, 3)).map(|_| ct(rng)).collect();
            format!("ARRAY[{}]", v.join(", "))
        }
    }
}

/// One `r5a` row's values for (id, a, b, n, iv, r, mr, doc, cs).
pub fn r5a_row(rng: &mut Rng, id: i64) -> String {
    let a = if rng.chance(0.15) { "NULL".to_string() } else { rng.range(-3, 20).to_string() };
    format!(
        "({id}, {a}, {}, {}, {}, {}, {}, {}, {})",
        q(rng, TEXTS5, 0.1),
        q(rng, NUMS5, 0.1),
        q(rng, IVS5, 0.1),
        q(rng, RANGES, 0.1),
        q(rng, MRANGES, 0.1),
        q(rng, DOCS, 0.1),
        cts(rng)
    )
}

/// The objects: collations, a composite type, `r5a` (generated columns, a tsvector, ranges,
/// composite arrays) with triggers logging into `r5log`, views over it with check options, a
/// view with INSTEAD OF triggers, a table with rules, a partitioned table `r5p` and a table
/// `r5px` to attach.
pub fn schema(rng: &mut Rng) -> Vec<String> {
    let coll = *rng.pick(COLLS5);
    let virt = if rng.chance(0.5) { "VIRTUAL" } else { "STORED" };
    let mut out = vec![
        "CREATE COLLATION r5ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false)".to_string(),
        "CREATE COLLATION r5ai (provider = icu, locale = 'und-u-ks-level1', deterministic = false)".into(),
        "CREATE COLLATION r5num (provider = icu, locale = 'en-u-kn-true')".into(),
        "CREATE TYPE r5ct AS (x int, y text)".into(),
        format!(
            "CREATE TABLE r5a (id int PRIMARY KEY, a int, b text COLLATE \"{coll}\", n numeric, iv interval, r int4range, mr int4multirange, doc text, cs r5ct[], g1 int GENERATED ALWAYS AS (a * 2) STORED, g2 text GENERATED ALWAYS AS (upper(b)) {virt}, tsv tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(doc, ''))) STORED)"
        ),
        "CREATE TABLE r5log (tg text, op text, k int, v text)".into(),
        "CREATE TABLE r5b (k int PRIMARY KEY, w text)".into(),
        "INSERT INTO r5b SELECT g, 'w' || g FROM generate_series(-3, 12) g".into(),
    ];
    let rows: Vec<String> = (1..=rng.range(3, 12)).map(|i| r5a_row(rng, i)).collect();
    out.push(format!("INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES {}", rows.join(", ")));
    for (p, sql) in [
        (0.5, "CREATE INDEX r5a_tsv ON r5a USING gin (tsv)".to_string()),
        (0.4, format!("CREATE INDEX r5a_b ON r5a (b COLLATE \"{}\")", rng.pick(COLLS5))),
        (0.3, "CREATE INDEX r5a_r ON r5a USING gist (r)".into()),
        (0.3, "CREATE INDEX r5a_mr ON r5a USING gist (mr)".into()),
        (0.3, "CREATE INDEX r5a_n ON r5a (n DESC NULLS LAST)".into()),
        (0.2, "CREATE INDEX r5a_iv ON r5a (iv)".into()),
    ] {
        if rng.chance(p) {
            out.push(sql);
        }
    }
    // Triggers: a BEFORE row trigger that fills and skips, AFTER row ones with WHEN, and an
    // AFTER statement trigger over transition tables.
    let skip = rng.range(0, 6);
    out.push(format!(
        "CREATE FUNCTION r5tb() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 7 = {skip} AND TG_OP = 'INSERT' THEN RETURN NULL; END IF; IF NEW.a IS NULL THEN NEW.a := -1; END IF; RETURN NEW; END $$"
    ));
    out.push("CREATE FUNCTION r5ta() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN INSERT INTO r5log VALUES (TG_NAME, TG_OP, OLD.id, OLD.a::text); RETURN OLD; END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, NEW.id, NEW.a::text || '/' || coalesce(NEW.g1::text, '-')); RETURN NEW; END $$".into());
    out.push("CREATE FUNCTION r5ts() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE c int; s text; BEGIN IF TG_OP = 'DELETE' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM ot; ELSIF TG_OP = 'INSERT' THEN SELECT count(*), string_agg(id::text, ',' ORDER BY id) INTO c, s FROM nt; ELSE SELECT count(*), string_agg(o.id::text || ':' || o.a::text || '>' || n.a::text, ',' ORDER BY o.id) INTO c, s FROM ot o JOIN nt n USING (id); END IF; INSERT INTO r5log VALUES (TG_NAME, TG_OP, c, s); RETURN NULL; END $$".into());
    if rng.chance(0.7) {
        let ev = *rng.pick(&["INSERT OR UPDATE", "INSERT", "UPDATE OF a, b"]);
        out.push(format!("CREATE TRIGGER r5tb BEFORE {ev} ON r5a FOR EACH ROW EXECUTE FUNCTION r5tb()"));
    }
    if rng.chance(0.6) {
        out.push("CREATE TRIGGER r5ta AFTER INSERT OR DELETE ON r5a FOR EACH ROW EXECUTE FUNCTION r5ta()".into());
        let when = *rng.pick(&["", " WHEN (OLD.a IS DISTINCT FROM NEW.a)", " WHEN (NEW.g1 > 0)", " WHEN (OLD.* IS DISTINCT FROM NEW.*)"]);
        out.push(format!("CREATE TRIGGER r5tu AFTER UPDATE ON r5a FOR EACH ROW{when} EXECUTE FUNCTION r5ta()"));
    }
    match rng.below(4) {
        0 => out.push("CREATE TRIGGER r5ts_i AFTER INSERT ON r5a REFERENCING NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts()".into()),
        1 => out.push("CREATE TRIGGER r5ts_u AFTER UPDATE ON r5a REFERENCING OLD TABLE AS ot NEW TABLE AS nt FOR EACH STATEMENT EXECUTE FUNCTION r5ts()".into()),
        2 => out.push("CREATE TRIGGER r5ts_d AFTER DELETE ON r5a REFERENCING OLD TABLE AS ot FOR EACH STATEMENT EXECUTE FUNCTION r5ts()".into()),
        _ => {}
    }
    out.extend(views(rng));
    out.extend(partitions(rng));
    if rng.chance(0.5) {
        out.push("ANALYZE r5a, r5b, r5p".into());
    }
    out
}

/// Updatable views with check options (one over the other), a join view with INSTEAD OF
/// triggers, and a table with conditional rules.
fn views(rng: &mut Rng) -> Vec<String> {
    let opt = |rng: &mut Rng| *rng.pick(&["", " WITH CHECK OPTION", " WITH LOCAL CHECK OPTION", " WITH CASCADED CHECK OPTION"]);
    let o1 = opt(rng);
    let o2 = opt(rng);
    let lo = rng.range(-2, 4);
    let hi = rng.range(8, 30);
    let mut out = vec![
        format!("CREATE VIEW r5v AS SELECT id, a, b, n FROM r5a WHERE a > {lo}{o1}"),
        format!("CREATE VIEW r5v2 AS SELECT id, a, b FROM r5v WHERE a < {hi}{o2}"),
        "CREATE VIEW r5w AS SELECT id, a, b, a + 1 AS a1, g1 FROM r5a WHERE id > 0".into(),
        "CREATE VIEW r5j AS SELECT x.id, x.a, y.w FROM r5a x JOIN r5b y ON y.k = x.a".into(),
        "CREATE FUNCTION r5jt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'INSERT' THEN INSERT INTO r5a (id, a) VALUES (NEW.id, NEW.a) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a; INSERT INTO r5log VALUES ('r5jt', TG_OP, NEW.id, NEW.w); RETURN NEW; ELSIF TG_OP = 'UPDATE' THEN UPDATE r5a SET a = NEW.a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w || '>' || NEW.w); RETURN NEW; END IF; DELETE FROM r5a WHERE id = OLD.id; INSERT INTO r5log VALUES ('r5jt', TG_OP, OLD.id, OLD.w); RETURN NULL; END $$".into(),
        "CREATE TRIGGER r5jt INSTEAD OF INSERT OR UPDATE OR DELETE ON r5j FOR EACH ROW EXECUTE FUNCTION r5jt()".into(),
        "CREATE TABLE r5r (id int, a int, t text)".into(),
        "INSERT INTO r5r SELECT g, g % 4 * 3 - 2, 'r' || g FROM generate_series(1, 8) g".into(),
    ];
    if rng.chance(0.7) {
        out.push("CREATE RULE r5r_ins AS ON INSERT TO r5r WHERE NEW.a < 0 DO INSTEAD INSERT INTO r5log VALUES ('rule', 'ins', NEW.id, NEW.t)".into());
    }
    if rng.chance(0.7) {
        let kind = *rng.pick(&["ALSO", "INSTEAD"]);
        out.push(format!("CREATE RULE r5r_upd AS ON UPDATE TO r5r DO {kind} INSERT INTO r5log VALUES ('rule', 'upd', OLD.id, OLD.a || '>' || NEW.a)"));
    }
    if rng.chance(0.6) {
        out.push("CREATE RULE r5r_del AS ON DELETE TO r5r WHERE OLD.a > 3 DO INSTEAD NOTHING".into());
    }
    if rng.chance(0.3) {
        out.push("CREATE RULE r5w_ins AS ON INSERT TO r5w DO ALSO INSERT INTO r5log VALUES ('rule', 'w', NEW.id, NEW.b)".into());
    }
    out
}

/// `r5p` partitioned by range on k (a list-partitioned partition now and then, a default
/// partition or not), and `r5px` with rows inside or outside the range it attaches to.
fn partitions(rng: &mut Rng) -> Vec<String> {
    let mut out = vec![
        "CREATE TABLE r5p (k int, v text, d date, PRIMARY KEY (k, v)) PARTITION BY RANGE (k)".to_string(),
        "CREATE TABLE r5p1 PARTITION OF r5p FOR VALUES FROM (MINVALUE) TO (10)".into(),
    ];
    if rng.chance(0.5) {
        out.push("CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (v)".into());
        out.push("CREATE TABLE r5p2a PARTITION OF r5p2 FOR VALUES IN ('a', 'b')".into());
        out.push("CREATE TABLE r5p2d PARTITION OF r5p2 DEFAULT".into());
    } else {
        out.push("CREATE TABLE r5p2 PARTITION OF r5p FOR VALUES FROM (10) TO (20)".into());
    }
    if rng.chance(0.4) {
        out.push("CREATE TABLE r5pd PARTITION OF r5p DEFAULT".into());
    }
    out.push("CREATE TABLE r5px (k int NOT NULL, v text NOT NULL, d date)".into());
    if rng.chance(0.3) {
        out.push("ALTER TABLE r5px ADD CONSTRAINT r5px_k CHECK (k >= 20 AND k < 30)".into());
    }
    let rows: Vec<String> = (0..rng.range(2, 9))
        .map(|i| format!("({}, '{}', {})", rng.range(-5, 19), ["a", "b", "c", "d"][i as usize % 4], q(rng, &["2024-02-29", "2025-01-01", "infinity"], 0.2)))
        .collect();
    out.push(format!("INSERT INTO r5p VALUES {} ON CONFLICT DO NOTHING", rows.join(", ")));
    let lo = if rng.chance(0.7) { 20 } else { 18 };
    let rows: Vec<String> = (0..rng.range(0, 5)).map(|i| format!("({}, 'x{i}', NULL)", rng.range(lo, 32))).collect();
    if !rows.is_empty() {
        out.push(format!("INSERT INTO r5px VALUES {}", rows.join(", ")));
    }
    out
}

fn query(sql: String, ordered: bool) -> Stmt {
    Stmt::Query(Query { sql, ordered, cols: Vec::<(String, Ty)>::new() })
}
fn dml(sql: String, table: &str) -> Stmt {
    Stmt::Dml { sql, table: table.into(), ordered: false }
}
/// What a DDL statement left: the table's columns (types, NOT NULL, generated, defaults) and
/// constraints (validated or not).
fn describe(t: &str) -> Vec<Stmt> {
    vec![
        query(format!("SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, a.attgenerated, pg_get_expr(d.adbin, d.adrelid), a.attcollation::regcollation::text FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = '{t}'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"), true),
        query(format!("SELECT conname, contype, convalidated, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = '{t}'::regclass ORDER BY conname COLLATE \"C\""), true),
    ]
}
fn log() -> Stmt {
    query("SELECT tg, op, k, v FROM r5log ORDER BY tg COLLATE \"C\", op COLLATE \"C\", k, v COLLATE \"C\"".into(), true)
}
fn partitions_now() -> Stmt {
    query("SELECT c.relname, p.relname, pg_get_expr(c.relpartbound, c.oid) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent ORDER BY 1 COLLATE \"C\"".into(), true)
}

/// A column of `r5a` some DDL changes, and the new names DDL adds.
const ACOLS: &[&str] = &["a", "b", "n", "iv", "r", "doc", "g1", "g2", "e1", "e2"];

/// DDL on `r5a`: type changes (with and without USING, over generated columns and indexes),
/// defaults, added / dropped columns (stored and virtual generated ones), constraints added
/// NOT VALID and validated later, NOT NULL, unique keys under the column's collation.
fn ddl_a(rng: &mut Rng) -> Vec<Stmt> {
    let c = *rng.pick(ACOLS);
    let e = *rng.pick(&["e1", "e2"]);
    let k = rng.range(1, 3);
    let sql = match rng.below(16) {
        0 => format!("ALTER TABLE r5a ALTER COLUMN a TYPE {}", rng.pick(&["bigint", "numeric(6,1)", "text", "smallint", "float8", "int"])),
        1 => format!("ALTER TABLE r5a ALTER COLUMN {c} TYPE {}", rng.pick(&["text", "varchar(2)", "int USING length({c}::text)", "numeric USING {c}::text::numeric", "bigint"])).replace("{c}", c),
        2 => format!("ALTER TABLE r5a ALTER COLUMN b TYPE text COLLATE \"{}\"", rng.pick(COLLS5)),
        3 => format!("ALTER TABLE r5a ALTER COLUMN {} SET DEFAULT {}", c, rng.pick(&["7", "'zz'", "1.5", "abs(-3)", "(SELECT 1)", "nextval('r5s')", "'1 day'::interval * 2"])),
        4 => format!("ALTER TABLE r5a ALTER COLUMN {c} DROP DEFAULT"),
        5 => format!("ALTER TABLE r5a ADD COLUMN {e} {}", rng.pick(&["int DEFAULT 5", "int DEFAULT 5 NOT NULL", "int NOT NULL", "text DEFAULT 'x' COLLATE \"r5ci\"", "numeric DEFAULT 1e30", "interval DEFAULT '1 mon'", "int[] DEFAULT '{1,2}'", "r5ct DEFAULT ROW(1, 'q')"])),
        6 => format!(
            "ALTER TABLE r5a ADD COLUMN {e} {} GENERATED ALWAYS AS ({}) {}",
            rng.pick(&["int", "text", "numeric", "bigint"]),
            rng.pick(&["a + id", "b || '!'", "n * 2", "length(doc)", "id * 1000000000", "a / (id - 3)", "(cs[1]).x"]),
            rng.pick(&["STORED", "VIRTUAL", ""])
        ),
        7 => format!("ALTER TABLE r5a DROP COLUMN {}{c}", if rng.chance(0.5) { "IF EXISTS " } else { "" }),
        8 => format!("ALTER TABLE r5a ALTER COLUMN {} SET EXPRESSION AS ({})", rng.pick(&["g1", "g2", "e1", "e2"]), rng.pick(&["a * 3", "a + 1", "lower(b)", "id / 0", "b"])),
        9 => format!("ALTER TABLE r5a ALTER COLUMN {} DROP EXPRESSION", rng.pick(&["g1", "g2", "e1"])),
        10 => format!("ALTER TABLE r5a ADD CONSTRAINT r5c{k} CHECK ({}){}", rng.pick(&["a > -2", "a IS NOT NULL", "length(b) < 3", "n < 1e50", "iv < '100 years'", "g1 < 30", "NOT isempty(r)"]), if rng.chance(0.6) { " NOT VALID" } else { "" }),
        11 => format!("ALTER TABLE r5a VALIDATE CONSTRAINT r5{}", rng.pick(&["c1", "c2", "c3", "f", "nn"])),
        12 => format!("ALTER TABLE r5a ADD CONSTRAINT r5f FOREIGN KEY (a) REFERENCES r5b (k){}", if rng.chance(0.6) { " NOT VALID" } else { "" }),
        13 => match rng.below(3) {
            0 => format!("ALTER TABLE r5a ALTER COLUMN {c} SET NOT NULL"),
            1 => format!("ALTER TABLE r5a ALTER COLUMN {c} DROP NOT NULL"),
            _ => format!("ALTER TABLE r5a ADD CONSTRAINT r5nn NOT NULL {c} NOT VALID"),
        },
        14 => format!("ALTER TABLE r5a ADD CONSTRAINT r5u UNIQUE {}({})", if rng.chance(0.3) { "NULLS NOT DISTINCT " } else { "" }, rng.pick(&["b", "a", "a, b", "g1"])),
        _ => format!("ALTER TABLE r5a DROP CONSTRAINT {}r5{}", if rng.chance(0.5) { "IF EXISTS " } else { "" }, rng.pick(&["c1", "c2", "c3", "f", "nn", "u"])),
    };
    let mut out = Vec::new();
    if sql.contains("r5s") {
        out.push(Stmt::Other("CREATE SEQUENCE IF NOT EXISTS r5s".into()));
    }
    out.push(dml(sql, "r5a"));
    out.extend(describe("r5a"));
    out
}

/// Partition DDL: attach (rows inside or outside the bound, a default partition holding rows
/// of the new bound), detach, re-attach, a partition created over existing rows.
fn ddl_p(rng: &mut Rng) -> Vec<Stmt> {
    let sql = match rng.below(7) {
        0 | 1 => format!("ALTER TABLE r5p ATTACH PARTITION r5px FOR VALUES FROM ({}) TO (30)", rng.pick(&["20", "18", "25"])),
        2 => format!("ALTER TABLE r5p DETACH PARTITION {}", rng.pick(&["r5p2", "r5px", "r5p1", "r5pd"])),
        3 => "ALTER TABLE r5p ATTACH PARTITION r5p2 FOR VALUES FROM (10) TO (20)".into(),
        4 => "CREATE TABLE r5p3 PARTITION OF r5p FOR VALUES FROM (30) TO (40)".into(),
        5 => "ALTER TABLE r5p ATTACH PARTITION r5pd DEFAULT".into(),
        _ => "CREATE TABLE r5pd PARTITION OF r5p DEFAULT".into(),
    };
    vec![dml(sql, "r5p"), partitions_now(), query("SELECT tableoid::regclass::text, k, v, d FROM r5p ORDER BY 2, 3 COLLATE \"C\"".into(), true)]
}

/// DML on `r5a` (through its triggers), with RETURNING of generated columns.
fn dml_a(rng: &mut Rng) -> Vec<Stmt> {
    let id = rng.range(0, 16);
    let cond = *rng.pick(&["a > 3", "id % 2 = 0", "b = 'a'", "a IS NULL", "true", "tsv @@ 'fat'", "r && '[2,4)'", "n > 0", "iv > '1 day'"]);
    let ret = *rng.pick(&["", " RETURNING id, a, g1, g2", " RETURNING *", " RETURNING old.a, new.a, new.g1"]);
    let sql = match rng.below(9) {
        0 => {
            let rows: Vec<String> = (0..rng.range(1, 3)).map(|i| r5a_row(rng, id + i)).collect();
            format!("INSERT INTO r5a (id, a, b, n, iv, r, mr, doc, cs) VALUES {}{ret}", rows.join(", "))
        }
        1 => format!(
            "INSERT INTO r5a (id, a, b) VALUES ({id}, {}, {}) ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a + r5a.a, b = EXCLUDED.b WHERE r5a.{cond}{ret}",
            rng.range(-2, 9),
            q(rng, TEXTS5, 0.1)
        ),
        2 => format!("UPDATE r5a SET a = a + {}, b = {} WHERE {cond}{ret}", rng.range(-2, 3), q(rng, TEXTS5, 0.1)),
        3 => format!("UPDATE r5a SET {} WHERE {cond}{ret}", rng.pick(&["n = n * 1e50", "n = n / 3", "iv = iv * 2", "iv = iv + '1 mon'", "cs[1].y = 'q'", "cs[2] = ROW(9, 'n')", "r = r * '[0,3)'", "mr = mr + '{[20,22)}'", "doc = doc || ' fat'", "g1 = 1", "g2 = DEFAULT, g1 = DEFAULT"])),
        4 => format!("DELETE FROM r5a WHERE {cond}{ret}"),
        5 => format!(
            "MERGE INTO r5a t USING (VALUES ({id}, {}), ({}, NULL)) s(k, x) ON t.id = s.k WHEN MATCHED AND s.x IS NULL THEN DELETE WHEN MATCHED THEN UPDATE SET a = s.x WHEN NOT MATCHED THEN INSERT (id, a, b) VALUES (s.k, s.x, 'm') RETURNING merge_action(), t.id, t.g1",
            rng.range(-1, 9),
            id + 1
        ),
        6 => format!("INSERT INTO r5a (id, a, g1) VALUES ({id}, 1, {})", rng.pick(&["DEFAULT", "2"])),
        7 => format!("INSERT INTO r5a SELECT id + 100, a, b FROM r5a WHERE {cond}{ret}"),
        _ => format!("UPDATE r5a x SET a = y.k FROM r5b y WHERE y.k = x.id - {} AND x.{cond}{ret}", rng.range(0, 3)),
    };
    vec![dml(sql, "r5a"), log()]
}

/// DML through the views (check options, a non-updatable column, an ALSO rule), the join
/// view's INSTEAD OF triggers, the rule table and the partitioned table (row movement).
fn dml_views(rng: &mut Rng) -> Vec<Stmt> {
    let id = rng.range(0, 16);
    let a = rng.range(-3, 32);
    let (sql, t) = match rng.below(11) {
        0 => (format!("INSERT INTO {} (id, a, b) VALUES ({id}, {a}, 'v'){}", rng.pick(&["r5v", "r5v2"]), rng.pick(&["", " RETURNING *"])), "r5a"),
        1 => (format!("UPDATE {} SET a = {a} WHERE id % 3 = {}", rng.pick(&["r5v", "r5v2"]), rng.range(0, 2)), "r5a"),
        2 => (format!("UPDATE r5v2 SET a = a + {} RETURNING id, a", rng.range(-5, 25)), "r5a"),
        3 => (format!("INSERT INTO r5w (id, a, b{}) VALUES ({id}, {a}, 'w'{})", if rng.chance(0.3) { ", a1" } else { "" }, if rng.chance(0.3) { ", 1" } else { "" }), "r5a"),
        4 => (format!("UPDATE r5w SET {} WHERE id < {id}", rng.pick(&["a = a1", "a1 = 3", "b = b || 'w'", "g1 = 0"])), "r5a"),
        5 => (format!("INSERT INTO r5j (id, a, w) VALUES ({id}, {a}, 'j') RETURNING *"), "r5a"),
        6 => (format!("UPDATE r5j SET a = a + 1, w = 'u' WHERE id % 2 = {}", rng.range(0, 1)), "r5a"),
        7 => (format!("DELETE FROM r5j WHERE a > {} RETURNING id", rng.range(-1, 8)), "r5a"),
        8 => (
            match rng.below(3) {
                0 => format!("INSERT INTO r5r VALUES ({id}, {}, 't'), ({}, {}, 'u')", rng.range(-3, 3), id + 1, rng.range(-3, 3)),
                1 => format!("UPDATE r5r SET a = a + {} WHERE id > {}{}", rng.range(-2, 2), rng.range(0, 6), rng.pick(&["", " RETURNING *"])),
                _ => format!("DELETE FROM r5r WHERE id % 2 = {}", rng.range(0, 1)),
            },
            "r5r",
        ),
        9 => (format!("UPDATE r5p SET k = k + {} WHERE v = '{}' RETURNING tableoid::regclass::text, k", rng.range(-12, 12), rng.pick(&["a", "b", "c", "x0"])), "r5p"),
        _ => (format!("INSERT INTO r5p VALUES ({}, '{}', '2024-01-01') ON CONFLICT (k, v) DO UPDATE SET d = EXCLUDED.d + 1 RETURNING tableoid::regclass::text, *", rng.range(-5, 45), rng.pick(&["a", "b", "c", "e"])), "r5p"),
    };
    vec![dml(sql, t), log()]
}

/// A transaction with cursors open across savepoints, rollbacks and DML: a cursor declared
/// after a savepoint rolled back is gone, one declared before keeps its position and its
/// snapshot, WITH HOLD ones outlive the commit, FOR UPDATE ones drive WHERE CURRENT OF.
fn cursors(rng: &mut Rng) -> Vec<Stmt> {
    let o = |s: &str| Stmt::Other(s.to_string());
    let src = *rng.pick(&[
        "SELECT id, a, b FROM r5a ORDER BY id",
        "SELECT id, a FROM r5a WHERE a > 0 ORDER BY a DESC, id",
        "SELECT k, v FROM r5p ORDER BY k, v",
        "SELECT id, a FROM r5a ORDER BY id FOR UPDATE",
        "SELECT x.id, y.w FROM r5a x LEFT JOIN r5b y ON y.k = x.a ORDER BY x.id",
    ]);
    let opts = *rng.pick(&["", "SCROLL ", "NO SCROLL ", "WITH HOLD ", "SCROLL WITH HOLD "]);
    let opts = if src.contains("FOR UPDATE") { opts.replace("SCROLL ", "").replace("NO ", "") } else { opts.to_string() };
    let opts = opts.trim_start();
    let mut out = vec![o("BEGIN"), o(&format!("DECLARE r5k {opts}CURSOR FOR {src}")), o(&format!("FETCH {} FROM r5k", rng.range(0, 3)))];
    out.push(o("SAVEPOINT r5s1"));
    let mut dmls = vec!["UPDATE r5a SET a = a + 10 WHERE id % 2 = 0", "DELETE FROM r5a WHERE id = 2", "INSERT INTO r5a (id, a) VALUES (50, 50)"];
    // (WHERE CURRENT OF only over a FOR UPDATE cursor: otherwise whether the cursor is simply
    // updatable depends on its plan for cursor_tuple_fraction, which the engine doesn't model.)
    if src.contains("FOR UPDATE") {
        dmls.push("UPDATE r5a SET a = -a WHERE CURRENT OF r5k");
        dmls.push("DELETE FROM r5a WHERE CURRENT OF r5k RETURNING id");
    }
    let upd = *rng.pick(&dmls);
    out.push(dml(upd.into(), "r5a"));
    if rng.chance(0.5) {
        out.push(o("DECLARE r5k2 CURSOR FOR SELECT id, a FROM r5a ORDER BY id"));
        out.push(o("FETCH 1 FROM r5k2"));
    }
    if rng.chance(0.3) {
        out.push(o("SELECT 1 / 0"));
    }
    let end_sp = *rng.pick(&["ROLLBACK TO SAVEPOINT r5s1", "RELEASE SAVEPOINT r5s1", "ROLLBACK TO r5s1"]);
    out.push(o(end_sp));
    for _ in 0..rng.range(1, 3) {
        // (Backwards only where the cursor says whether it may: without [NO] SCROLL that
        // depends on the plan for cursor_tuple_fraction, which the engine doesn't model.)
        let f = if opts.contains("SCROLL") {
            *rng.pick(&["FETCH NEXT FROM r5k", "FETCH PRIOR FROM r5k", "FETCH ABSOLUTE 2 FROM r5k", "FETCH RELATIVE -1 FROM r5k", "FETCH ALL FROM r5k", "MOVE LAST IN r5k", "FETCH BACKWARD 2 FROM r5k", "FETCH 1 FROM r5k2", "MOVE FORWARD ALL IN r5k2"])
        } else {
            *rng.pick(&["FETCH NEXT FROM r5k", "FETCH ALL FROM r5k", "FETCH 2 FROM r5k", "MOVE 1 IN r5k", "FETCH RELATIVE 1 FROM r5k", "FETCH 1 FROM r5k2", "MOVE FORWARD ALL IN r5k2"])
        };
        out.push(o(f));
    }
    if rng.chance(0.4) {
        out.push(o("SELECT name, statement, is_holdable, is_binary, is_scrollable OR statement NOT LIKE '%SCROLL%' FROM pg_cursors ORDER BY name"));
    }
    out.push(o(rng.pick(&["COMMIT", "ROLLBACK", "COMMIT AND CHAIN"])));
    out.push(o("FETCH ALL FROM r5k"));
    out.push(o("COMMIT"));
    out.push(o("CLOSE ALL"));
    out.push(query("SELECT id, a, b FROM r5a ORDER BY id".into(), true));
    out
}

/// Parameter types and values of every kind: valid, NULL, out of range or malformed.
const PTYPES: &[(&str, &[&str])] = &[
    ("int2", &["-32768", "32767", "'32768'", "1.5"]),
    ("int4", &["2147483647", "'-2147483648'", "'1e3'", "' 7 '"]),
    ("int8", &["-9223372036854775808", "'9223372036854775808'", "'0x1F'", "'1_000'"]),
    ("numeric", &["'1e-30'", "'NaN'", "1e100", "'-0'"]),
    ("numeric(5,2)", &["123.456", "'999.999'", "'1e2'"]),
    ("float4", &["'1.5e38'", "'3.4e39'", "'-0'", "'NaN'"]),
    ("float8", &["'-Infinity'", "'1e-320'", "'0x10'", "0.1"]),
    ("text", &["'a''b'", "''", "E'\\\\x'", "U&'\\00e9'"]),
    ("varchar(3)", &["'abcd'", "'ab   '", "'é'"]),
    ("char(2)", &["'a'", "'abc'", "'  '"]),
    ("\"char\"", &["'xyz'", "''"]),
    ("name", &["'n'", "repeat('n', 70)"]),
    ("bool", &["'yes'", "'of'", "'tru'", "1"]),
    ("date", &["'2024-02-29'", "'2023-02-29'", "'infinity'", "'J2451545'"]),
    ("time", &["'24:00'", "'24:00:01'", "'1:2:3.4567891'"]),
    ("timetz", &["'12:00+05'", "'12:00 America/New_York'"]),
    ("timestamp", &["'infinity'", "'2025-03-09 02:30'", "'294277-01-01'"]),
    ("timestamptz", &["'2025-03-09 02:30 America/New_York'", "'epoch'", "'2025-11-02 01:30 EST5EDT'"]),
    ("interval", &["'P1Y2M'", "'1 day -1 sec'", "'178000000 years'", "'@ 1 hour ago'"]),
    ("interval(0)", &["'1.6 sec'", "'P0.5D'"]),
    ("bytea", &["'\\x00ff'", "'\\xzz'", "'abc\\\\'"]),
    ("uuid", &["'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11'", "'{a0eebc999c0b4ef8bb6d6bb9bd380a11}'", "'a0ee'"]),
    ("jsonb", &["'{\"a\": 1, \"a\": 2}'", "'[1, 2.50]'", "'nul'"]),
    ("json", &["'{\"a\": 1, \"a\": 2}'", "'  [1 ]'"]),
    ("int4[]", &["'{1,NULL}'", "'{{1},{2}}'", "'[0:1]={5,6}'", "'{1,'"]),
    ("text[]", &["'{a,\"b c\",NULL,\"NULL\"}'", "ARRAY['x']"]),
    ("int4range", &["'[1,5)'", "'(5,1)'", "'empty'"]),
    ("int4multirange", &["'{[1,2],[4,5)}'", "'{[3,1)}'"]),
    ("numrange", &["'[1.5,2.5]'", "'(,)'"]),
    ("tstzrange", &["'[2025-01-01, infinity)'"]),
    ("inet", &["'192.168.0.1/24'", "'::1'", "'1.2.3'"]),
    ("cidr", &["'10.0.0.0/8'", "'10.0.0.1/8'"]),
    ("macaddr", &["'08:00:2b:01:02:03'", "'0800.2b01.0203'"]),
    ("money", &["'12.34'", "'-$1,000.5'"]),
    ("bit(4)", &["B'1010'", "B'10'", "'101'"]),
    ("varbit", &["B'101'", "'x1f'"]),
    ("oid", &["4294967295", "'-1'", "'4294967296'"]),
    ("point", &["'(1,2)'", "'1,2'"]),
    ("tsvector", &["'a:1 b:2A fat:3'", "'a:0'"]),
    ("tsquery", &["'a & !b'", "'fat <-> rat'", "'&'"]),
    ("r5ct", &["ROW(1, 'x')", "'(2,y)'", "'(a,b)'"]),
    ("r5ct[]", &["ARRAY[ROW(1, 'x')::r5ct]", "'{\"(1,a)\",NULL}'"]),
    ("regclass", &["'r5a'", "'r5nope'"]),
    ("regtype", &["'int'", "'double precision[]'"]),
];

/// Prepared statements: parameters of every type (declared or inferred) and values valid,
/// NULL or not; a statement kept across later DDL (its result type changing); DML.
fn prepared(rng: &mut Rng) -> Vec<Stmt> {
    let o = |s: String| Stmt::Other(s);
    let mut out = Vec::new();
    match rng.below(5) {
        0 | 1 => {
            let ps: Vec<&(&str, &[&str])> = (0..rng.range(1, 4)).map(|_| rng.pick(PTYPES)).collect();
            let tys: Vec<&str> = ps.iter().map(|p| p.0).collect();
            let sel: Vec<String> = (1..=ps.len()).map(|i| format!("${i}, pg_typeof(${i})::text, ${i}::text")).collect();
            let name = format!("r5q{}", rng.range(1, 3));
            out.push(o(format!("PREPARE {name}({}) AS SELECT {}", tys.join(", "), sel.join(", "))));
            for _ in 0..rng.range(1, 3) {
                let vals: Vec<String> = ps.iter().map(|p| if rng.chance(0.15) { "NULL".to_string() } else { rng.pick(p.1).to_string() }).collect();
                out.push(o(format!("EXECUTE {name}({})", vals.join(", "))));
            }
            if rng.chance(0.7) {
                out.push(o(format!("DEALLOCATE {name}")));
            }
        }
        2 => out.push(o(format!("PREPARE r5qa(int) AS SELECT * FROM {} WHERE id >= $1 ORDER BY id", rng.pick(&["r5a", "r5v", "r5w"])))),
        3 => {
            out.push(o(format!("EXECUTE r5qa({})", rng.range(0, 8))));
            out.push(o("SELECT name, parameter_types::text, result_types::text, from_sql FROM pg_prepared_statements ORDER BY name".into()));
        }
        _ => {
            let (p, args) = *rng.pick(&[
                ("PREPARE r5qu AS SELECT $1 || 'x', $2 + 1, $3 = ANY ('{1,2}'::int[])", "('a', 2, 1)"),
                ("PREPARE r5qu AS SELECT $1 || 'x', $2 + 1, $3 = ANY ('{1,2}'::int[])", "(NULL, '2.5', '1')"),
                ("PREPARE r5qu AS SELECT count(*) FROM r5a WHERE b = $1 AND r @> $2", "('a', 3)"),
                ("PREPARE r5qu AS SELECT $1::int + $1", "('7')"),
                ("PREPARE r5qu (int, text) AS INSERT INTO r5a (id, a, b) VALUES ($1, $1 * 2, $2) RETURNING g1, g2", "(30, 'p')"),
                ("PREPARE r5qu (numeric, interval) AS UPDATE r5a SET n = $1, iv = $2 WHERE id = 1 RETURNING n, iv", "(1e300, '-178000000 years')"),
                ("PREPARE r5qu (int4range[]) AS SELECT $1, $1[1] * '[2,3]'", "('{\"[1,4)\",empty}')"),
            ]);
            out.push(o(p.into()));
            let ex = format!("EXECUTE r5qu{args}");
            out.push(if p.contains(" r5a ") && !p.contains("SELECT count") { dml(ex, "r5a") } else { o(ex) });
            out.push(o("DEALLOCATE r5qu".into()));
        }
    }
    out
}

/// COPY out (text / csv with options, a query, generated columns left out without a list) and
/// in (good and bad lines, defaults, NULL markers, ON_ERROR ignore, header matching, triggers).
fn copy(rng: &mut Rng) -> Vec<Stmt> {
    let fmt = *rng.pick(&[
        "",
        " (FORMAT csv)",
        " (FORMAT csv, HEADER true, DELIMITER ';', NULL 'N')",
        " (FORMAT csv, FORCE_QUOTE *, QUOTE '''', ESCAPE '\\')",
        " (FORMAT text, NULL '', DELIMITER '|')",
        " (FORMAT csv, HEADER, FORCE_NOT_NULL (b))",
        " (FORMAT text, HEADER true)",
    ]);
    if rng.chance(0.5) {
        let src = *rng.pick(&[
            "r5a",
            "r5a (id, a, b, g1, g2)",
            "r5a (id, n, iv, r, mr, cs)",
            "(SELECT id, b, tsv, n FROM r5a ORDER BY id)",
            "r5p",
            "(SELECT * FROM r5v2 WHERE a > 0)",
            "r5j",
        ]);
        return vec![Stmt::Other(format!("COPY {src} TO STDOUT{fmt}"))];
    }
    let csv = fmt.contains("csv");
    let delim = if fmt.contains("';'") { ";" } else if fmt.contains("'|'") { "|" } else if csv { "," } else { "\t" };
    let mut lines = Vec::new();
    if fmt.contains("HEADER") {
        lines.push(["id", "a", "b"].join(delim));
    }
    for _ in 0..rng.range(1, 4) {
        let id = rng.range(0, 40).to_string();
        let a = rng.pick(&["1", "-2", "\\N", "", "N", "x", "99999999999", "3"]).to_string();
        let b = rng.pick(&["a", "\"q,\"\"\"", "é", "\\N", "", "tab\\tx", "N", "ß"]).to_string();
        lines.push([id, a, b].join(delim));
    }
    if rng.chance(0.2) {
        lines.push("1".into());
    }
    let extra = *rng.pick(&["", ", ON_ERROR ignore", ", ON_ERROR ignore, LOG_VERBOSITY verbose", ", DEFAULT 'D'", ", ON_ERROR ignore, REJECT_LIMIT 1"]);
    let opts = match (fmt, extra) {
        ("", "") => String::new(),
        ("", e) => format!(" ({})", &e[2..]),
        (f, e) => format!("{}{e})", &f[..f.len() - 1]),
    };
    let sql = format!("COPY r5a (id, a, b) FROM STDIN{opts}\n\\copy-data\n{}\n", lines.join("\n"));
    vec![dml(sql, "r5a"), log()]
}

/// Collations (ICU, libc, builtin, nondeterministic) in ORDER BY, GROUP BY, DISTINCT,
/// comparisons, pattern matching, indexes and set operations.
fn collations(rng: &mut Rng) -> Stmt {
    let c = *rng.pick(COLLS5);
    let c2 = *rng.pick(COLLS5);
    let t = q(rng, TEXTS5, 0.0);
    let dir = *rng.pick(&["", " DESC", " NULLS FIRST", " DESC NULLS LAST"]);
    let (sql, ordered) = match rng.below(12) {
        0 => (format!("SELECT b, id FROM r5a ORDER BY b COLLATE \"{c}\"{dir}, id"), true),
        1 => (format!("SELECT count(*), min(id), min(b COLLATE \"C\") FROM r5a GROUP BY b COLLATE \"{c}\" ORDER BY 2"), true),
        2 => (format!("SELECT count(DISTINCT b COLLATE \"{c}\"), count(DISTINCT (b COLLATE \"{c}\", a)) FROM r5a"), false),
        3 => (format!("SELECT id, b < {t} COLLATE \"{c}\", b = {t} COLLATE \"{c}\", b >= {t} COLLATE \"{c}\" FROM r5a ORDER BY id"), true),
        4 => (format!("SELECT id FROM r5a WHERE b COLLATE \"{c}\" {} {t} ORDER BY id", rng.pick(&["LIKE", "ILIKE", "~", "~*", "NOT LIKE", "SIMILAR TO"])), true),
        5 => (format!("SELECT string_agg(b, ',' ORDER BY b COLLATE \"{c}\"{dir}, id), count(DISTINCT b COLLATE \"{c2}\") FROM r5a"), true),
        6 => (format!("SELECT x.id, y.id FROM r5a x JOIN r5a y ON x.b = y.b COLLATE \"{c}\" WHERE x.id < y.id ORDER BY 1, 2"), true),
        7 => (format!("SELECT count(*) FROM (SELECT b COLLATE \"{c}\" FROM r5a UNION SELECT {t}) s"), false),
        8 => (format!("SELECT upper(b COLLATE \"{c}\"), lower(b COLLATE \"{c}\"), initcap(b COLLATE \"{c}\"), casefold(b COLLATE \"{c}\") FROM r5a ORDER BY id"), true),
        9 => (format!("SELECT id, rank() OVER (ORDER BY b COLLATE \"{c}\"{dir}) FROM r5a ORDER BY 1"), true),
        10 => (format!("SELECT min(b COLLATE \"C\"), count(*) FROM r5a GROUP BY ROLLUP (b COLLATE \"{c}\") ORDER BY 2, 1"), true),
        _ => (format!("SELECT id, b FROM r5a WHERE b IN ({t}, 'A' COLLATE \"{c}\") OR b > {t} ORDER BY b, id"), true),
    };
    query(sql, ordered)
}

/// Full text search: parsing, stemming, queries of every kind, ranking, headlines, weights,
/// over the stored tsvector (and its GIN index) and computed ones.
fn fts(rng: &mut Rng) -> Stmt {
    let cfg = *rng.pick(&["english", "simple", "english", "german"]);
    let tq = *rng.pick(&["fat & rat", "fat | cat", "!fat", "fat <-> cat", "fat <2> rat", "run:*", "fox & !dog", "sat:B", "supernova", "'fat cat'", "(fat | run) & !(cat <-> sat)"]);
    let wq = *rng.pick(&["fat rat", "\"fat cat\" -dog", "fox or dog", "-run", "\"the and\"", "fat -", "cat or"]);
    let sql = match rng.below(11) {
        0 => format!("SELECT id FROM r5a WHERE tsv @@ to_tsquery('{cfg}', '{tq}') ORDER BY id"),
        1 => format!("SELECT id FROM r5a WHERE tsv @@ {}('{cfg}', '{wq}') ORDER BY id", rng.pick(&["websearch_to_tsquery", "plainto_tsquery", "phraseto_tsquery"])),
        2 => format!("SELECT id, ts_rank(tsv, q)::numeric(10,6), ts_rank_cd(tsv, q)::numeric(10,6), ts_rank('{{0.1, 0.2, 0.4, 1.0}}', tsv, q, {})::numeric(10,6) FROM r5a, to_tsquery('english', '{tq}') q WHERE tsv @@ q ORDER BY 2 DESC, id", rng.pick(&["0", "1", "2", "4", "8", "32"])),
        3 => format!("SELECT id, to_tsvector('{cfg}', doc)::text, strip(to_tsvector('{cfg}', doc))::text FROM r5a ORDER BY id"),
        4 => format!("SELECT id, ts_headline('{cfg}', doc, to_tsquery('{cfg}', '{tq}'){}) FROM r5a WHERE doc IS NOT NULL ORDER BY id", rng.pick(&["", ", 'MaxWords=3, MinWords=1'", ", 'StartSel=<, StopSel=>, HighlightAll=true'", ", 'MaxFragments=2, FragmentDelimiter=|'"])),
        5 => format!("SELECT {}('{cfg}', '{wq}')::text", rng.pick(&["websearch_to_tsquery", "plainto_tsquery", "phraseto_tsquery", "to_tsquery"])),
        6 => "SELECT id, (setweight(tsv, 'A') || to_tsvector('simple', coalesce(b, '')))::text, ts_delete(tsv, ARRAY['fat', 'cat'])::text, tsvector_to_array(tsv)::text FROM r5a ORDER BY id".into(),
        7 => format!("SELECT word, ndoc, nentry FROM ts_stat('SELECT tsv FROM r5a') ORDER BY ndoc DESC, nentry DESC, word LIMIT {}", rng.range(1, 8)),
        8 => format!("SELECT alias, token, lexemes::text FROM ts_debug('{cfg}', 'fox-hound e-mail user@example.com 3.14 Running') ORDER BY 2, 1"),
        9 => format!("SELECT numnode(q), querytree(q), q::text, (q && 'cat'::tsquery)::text, (!! q)::text, tsquery_phrase(q, 'mat'::tsquery, 2)::text FROM to_tsquery('{cfg}', '{tq}') q"),
        _ => format!("SELECT count(*), string_agg(id::text, ',' ORDER BY id) FROM r5a WHERE to_tsvector('{cfg}', coalesce(doc, '') || ' ' || coalesce(b, '')) @@ to_tsquery('{cfg}', '{tq}')"),
    };
    query(sql, true)
}

/// Range and multirange operators, functions and aggregates (with GiST indexes on `r5a`).
fn ranges(rng: &mut Rng) -> Stmt {
    let op = *rng.pick(&["&&", "@>", "<@", "<<", ">>", "&<", "&>", "-|-", "=", "<", ">="]);
    let lit = q(rng, RANGES, 0.0);
    let mlit = q(rng, MRANGES, 0.0);
    let bin = *rng.pick(&["+", "*", "-"]);
    let sql = match rng.below(10) {
        0 => format!("SELECT id, r {op} {lit}, mr {op} {mlit}, r {op} {mlit}, mr {op} {lit} FROM r5a ORDER BY id"),
        1 => format!("SELECT id, (r {bin} {lit})::text FROM r5a ORDER BY id"),
        2 => format!("SELECT id, (mr {bin} {mlit})::text, (mr {bin} r::int4multirange)::text FROM r5a ORDER BY id"),
        3 => "SELECT range_agg(r)::text, range_intersect_agg(r)::text, range_agg(mr)::text, range_intersect_agg(mr)::text FROM r5a".into(),
        4 => "SELECT id, lower(r), upper(r), isempty(r), lower_inc(r), upper_inf(r), lower(mr), upper_inf(mr), range_merge(mr)::text FROM r5a ORDER BY id".into(),
        5 => format!("SELECT id FROM r5a WHERE r {op} {lit} ORDER BY id"),
        6 => "SELECT id, u::text, o FROM r5a, unnest(mr) WITH ORDINALITY u(x, o) ORDER BY 1, 3".into(),
        7 => format!("SELECT r::text, count(*) FROM r5a GROUP BY r ORDER BY r{}", rng.pick(&["", " DESC"])),
        8 => format!("SELECT id, multirange(r)::text, int4multirange({lit}, {lit})::text, ({lit}::int4range + {lit})::text FROM r5a ORDER BY id"),
        _ => format!("SELECT numrange({}, {})::text, daterange('2024-02-28', '2024-03-01', '{}')::text, tstzrange('2025-03-09 01:00-05', 'infinity') @> '2025-03-09 03:30-04'::timestamptz", rng.pick(&["1.5", "NULL", "-1e30"]), rng.pick(&["2", "1.5", "NULL"]), rng.pick(&["[]", "()", "(]"])),
    };
    query(sql, true)
}

/// Arrays of composites: element and field access, unnest, aggregation into them,
/// comparisons, containment, sorting, and casts through text.
fn composites(rng: &mut Rng) -> Stmt {
    let lit = ct(rng);
    let op = *rng.pick(&["=", "<>", "<", ">=", "@>", "<@", "&&"]);
    let sql = match rng.below(9) {
        0 => "SELECT id, cs[1], (cs[1]).x, (cs[2]).y, cardinality(cs), array_length(cs, 1) FROM r5a ORDER BY id".into(),
        1 => "SELECT id, u.x, u.y, o FROM r5a, unnest(cs) WITH ORDINALITY u(x, y, o) ORDER BY 1, 4".into(),
        2 => format!("SELECT id, cs {op} ARRAY[{lit}] FROM r5a ORDER BY id"),
        3 => format!("SELECT id FROM r5a WHERE {lit} = ANY (cs) OR {lit} > ALL (cs) ORDER BY id"),
        4 => format!("SELECT id, cs::text FROM r5a ORDER BY cs{}, id", rng.pick(&["", " DESC", " NULLS FIRST"])),
        5 => "SELECT array_agg(ROW(id, b)::r5ct ORDER BY id)::text, array_agg(cs[1] ORDER BY id) FILTER (WHERE cs IS NOT NULL)::text FROM r5a".into(),
        6 => format!("SELECT id, array_append(cs, {lit})::text, array_position(cs, {lit}), array_remove(cs, {lit})::text FROM r5a ORDER BY id"),
        7 => "SELECT (c).x, count(*), string_agg((c).y, ',' ORDER BY (c).y COLLATE \"C\") FROM (SELECT unnest(cs) c FROM r5a) s GROUP BY (c).x ORDER BY 1 NULLS FIRST".into(),
        _ => format!("SELECT id, (cs::text)::r5ct[] = cs, {lit}::text::r5ct, row_to_json(cs[1])::text, to_jsonb(cs)::text FROM r5a ORDER BY id"),
    };
    query(sql, true)
}

/// Large numerics: precision and scale of +, *, /, power, sqrt, exp / ln, rounding, casts,
/// overflow into int8 / float8, aggregates over huge and tiny values.
fn numerics(rng: &mut Rng) -> Stmt {
    let x = *rng.pick(NUMS5);
    let k = rng.range(-5, 40);
    let sql = match rng.below(10) {
        0 => format!("SELECT id, n + '{x}', n * '{x}', n - n, n / 3, n % 7 FROM r5a ORDER BY id"),
        1 => format!("SELECT id, round(n, {k}), trunc(n, {k}), ceil(n), floor(n), scale(n), min_scale(n), trim_scale(n) FROM r5a ORDER BY id"),
        2 => "SELECT id, n::float8, n::int8, n::int4, n::numeric(12, 4), n::text::float4 FROM r5a ORDER BY id".into(),
        3 => "SELECT sum(n), avg(n), stddev_samp(n), var_pop(n), max(n), min(n), count(n) FROM r5a".into(),
        4 => format!("SELECT power(10::numeric, {}), power(2::numeric, {})::text, sqrt(2::numeric), exp({}::numeric), ln({x}::numeric)", rng.pick(&["300", "-300", "1000", "131072", "0.5"]), rng.pick(&["1000", "-1074", "0.5"]), rng.pick(&["1", "100", "-100", "6000"])),
        5 => format!("SELECT factorial({}), gcd({x}::numeric, 12), lcm(6::numeric, {}), div({x}::numeric, 7), mod({x}::numeric, 0.3)", rng.pick(&["20", "30", "100", "-1"]), rng.pick(&["4", "1e40", "0"])),
        6 => format!("SELECT 1::numeric / 3, 1e-130::numeric / 7, 10::numeric ^ {}, (2::numeric ^ 0.5)::numeric(30, 28), 9.99e100 * 9.99e100", rng.pick(&["-20", "20", "1000"])),
        7 => format!("SELECT to_char({x}::numeric, '{}'), width_bucket({x}::numeric, -1e40, 1e40, 4)", rng.pick(&["9.99EEEE", "FM999999999999999999999999.999", "S9G999D99", "RN"])),
        8 => "SELECT id, n, rank() OVER (ORDER BY n), sum(n) OVER (ORDER BY n NULLS FIRST, id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM r5a ORDER BY id".into(),
        _ => format!("SELECT n FROM r5a WHERE n {} '{x}'::numeric ORDER BY n NULLS LAST", rng.pick(&["<", "=", ">", "<>"])),
    };
    query(sql, true)
}

/// Intervals at their limits: arithmetic that overflows, infinities, justify, extract, sums
/// and averages, comparisons of unequal-looking equal values, date / timestamp arithmetic.
fn intervals(rng: &mut Rng) -> Stmt {
    let x = *rng.pick(IVS5);
    let f = *rng.pick(&["2", "0.5", "-1", "1e10", "'NaN'", "0", "1.5e-7"]);
    let sql = match rng.below(10) {
        0 => format!("SELECT id, iv + '{x}', iv - '{x}', iv * {f}, iv / {f} FROM r5a ORDER BY id"),
        1 => "SELECT sum(iv), avg(iv), max(iv), min(iv), count(DISTINCT iv) FROM r5a".into(),
        2 => "SELECT id, extract(epoch FROM iv), extract(day FROM iv), date_part('month', iv), extract(millennium FROM iv), isfinite(iv) FROM r5a ORDER BY id".into(),
        3 => "SELECT id, justify_hours(iv), justify_days(iv), justify_interval(iv), -iv FROM r5a ORDER BY id".into(),
        4 => "SELECT id, timestamp '2000-01-01' + iv, date '2024-02-29' - iv, timestamptz '2025-03-09 01:00 America/New_York' + iv, time '23:00' + iv FROM r5a ORDER BY id".into(),
        5 => format!("SELECT iv, count(*) FROM r5a GROUP BY iv ORDER BY iv{}", rng.pick(&["", " DESC"])),
        6 => format!("SELECT id FROM r5a WHERE iv {} '{x}' ORDER BY id", rng.pick(&["<", "=", ">", "<>", ">="])),
        7 => format!("SELECT make_interval(years => {}, days => {}), interval '{x}' * 1e300, interval '1 mon' = interval '30 days', interval '24 hours' = interval '1 day'", rng.pick(&["178000000", "1", "-178000001"]), rng.pick(&["2147483647", "1", "0"])),
        8 => "SELECT id, iv, sum(iv) OVER (ORDER BY iv NULLS FIRST, id), avg(iv) OVER (ORDER BY id ROWS 1 PRECEDING) FROM r5a ORDER BY id".into(),
        _ => format!("SELECT id, to_char(iv, '{}'), iv::text, date_bin('{}', timestamp '2025-01-01 13:17', timestamp '2000-01-01') FROM r5a ORDER BY id", rng.pick(&["HH24:MI:SS.US", "YYYY MM DD", "FMDD \"days\""]), rng.pick(&["15 minutes", "1 day", "1 mon", "0"])),
    };
    query(sql, true)
}

impl super::Gen<'_> {
    /// One round 5 statement (several for a DDL step, a transaction or a prepared statement).
    pub(super) fn round5_stmt(&mut self) -> Vec<Stmt> {
        let rng = &mut self.rng;
        match rng.below(20) {
            0 | 1 => ddl_a(rng),
            2 => ddl_p(rng),
            3..=5 => dml_a(rng),
            6 | 7 => dml_views(rng),
            8 => cursors(rng),
            9 | 10 => prepared(rng),
            11 | 12 => copy(rng),
            13 => vec![collations(rng)],
            14 => vec![fts(rng)],
            15 => vec![ranges(rng)],
            16 => vec![composites(rng)],
            17 => vec![numerics(rng)],
            18 => vec![intervals(rng)],
            _ => vec![collations(rng), fts(rng)],
        }
    }
}
