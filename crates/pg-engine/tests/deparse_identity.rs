//! The native deparser (`sql::deparse`) gives exactly the text libpg_query's deparser gave
//! (`pg_query::deparse` with `fix_xmlparse`), which stored definitions keep: every statement
//! and every expression in it of the crate's goldens (`tests/sql/*.sql`), a fixed corpus
//! below, and PostgreSQL's regression SQL when it is there (`DEPARSE_REGRESS_SQL`, or
//! `$PGREGRESS_DIR/src-repo/src/test/regress/sql`).

use pg_engine::sql::binder::ast;
use pg_engine::sql::deparse;
use pg_query::NodeEnum as E;
use pg_query::protobuf::Node;
use std::path::{Path, PathBuf};

/// Shapes the goldens may not have.
const CORPUS: &str = r#"
SELECT xmlparse(document '<a/>' PRESERVE WHITESPACE), xmlparse(content 'x' STRIP WHITESPACE), xmlparse(content xmlparse(document 'y' PRESERVE WHITESPACE));
SELECT xmlelement(name "Foo", xmlattributes(1 AS a, b), 'c'), xmlforest(a, b AS "B"), xmlpi(name php, 'x'), xmlroot(x, version '1.0', standalone yes), xmlroot(x, version no value), xmlserialize(document x AS text INDENT), x IS DOCUMENT, xmlexists('//a' PASSING BY REF x), xmlconcat(a, b);
SELECT * FROM xmltable(xmlnamespaces('http://x' AS x, DEFAULT 'http://y'), '/r' PASSING d COLUMNS a int PATH 'a' DEFAULT 1 NOT NULL, o FOR ORDINALITY) AS t;
SELECT JSON_OBJECT('a': 1 FORMAT JSON, 'b' VALUE 2 ABSENT ON NULL WITH UNIQUE KEYS RETURNING jsonb), JSON_ARRAY(1, 2 NULL ON NULL RETURNING text FORMAT JSON), JSON_ARRAY(SELECT 1), JSON(t WITH UNIQUE KEYS), JSON_SCALAR(1), JSON_SERIALIZE(j RETURNING bytea);
SELECT JSON_OBJECTAGG(k: v ABSENT ON NULL) FILTER (WHERE k > 1) OVER w, JSON_ARRAYAGG(v ORDER BY v DESC NULL ON NULL) OVER (PARTITION BY k) FROM t WINDOW w AS (ORDER BY k);
SELECT JSON_QUERY(j, '$.a' PASSING 1 AS x RETURNING jsonb WITH CONDITIONAL WRAPPER OMIT QUOTES EMPTY ARRAY ON EMPTY ERROR ON ERROR), JSON_VALUE(j, '$' DEFAULT 1 ON EMPTY), JSON_EXISTS(j, '$' UNKNOWN ON ERROR), j IS JSON OBJECT WITH UNIQUE KEYS, j IS NOT JSON SCALAR;
SELECT * FROM JSON_TABLE(j, '$[*]' AS p PASSING 1 AS x COLUMNS (id FOR ORDINALITY, a int PATH '$.a' WITHOUT WRAPPER, b jsonb FORMAT JSON PATH '$.b' WITH WRAPPER KEEP QUOTES, e bool EXISTS PATH '$.e', NESTED PATH '$.n[*]' AS n COLUMNS (c text)) ERROR ON ERROR) jt;
SELECT a OPERATOR(pg_catalog.+) b, OPERATOR(s.~) a, a = ANY(b), a <> ALL(ARRAY[1]), a LIKE ANY (SELECT 1), a NOT ILIKE ALL (SELECT 'x'), a OPERATOR(pg_catalog.=) ANY (SELECT 1), (a, b) IN (SELECT 1, 2), a NOT IN (1, 2);
SELECT a BETWEEN SYMMETRIC b + 1 AND c, a NOT BETWEEN (b IS NULL) AND c, a SIMILAR TO 'x' ESCAPE '#', a NOT SIMILAR TO b, a IS DISTINCT FROM b, NULLIF(a, b), -a, - -a, +(a), NOT NOT a, (a OR b) AND (c OR d), NOT (a AND b), a IS NOT TRUE, a IS UNKNOWN;
SELECT E'a\\b''c', 'it''s', B'1010', X'FF', 1.5e10, -1, (-1)::int, 1.5::numeric, '1'::bool, 't'::boolean, 'x'::bpchar, 'x'::char(3), point '(1,2)', interval '1' day to second(3), '1'::interval hour, '1'::timestamptz(3), '1'::time(2) with time zone, 1::double precision, 1::"int4", '{1}'::int[][3], x::text COLLATE "C", a COLLATE "de_DE" < b;
SELECT CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END, CASE x WHEN 1 THEN 'a' END, ROW(1, 2), (1, 2), ARRAY[ARRAY[1]], ARRAY(SELECT 1), (SELECT 1), EXISTS (SELECT 1), (x).f, (x).*, x[1:2], x[:1], $1[2], ($1).a, (f(x)).y, GROUPING(a, b), COALESCE(a, b), GREATEST(1, 2), LEAST(1, 2);
SELECT current_date, current_time(3), current_timestamp, localtime(2), localtimestamp, current_role, current_user, user, session_user, current_catalog, current_schema, SYSTEM_USER;
SELECT substring(a FROM 1 FOR 2), substring(a, 1), position('a' IN b), overlay(a PLACING b FROM 1 FOR 2), overlay(a PLACING b FROM 1), trim(both 'x' FROM a), trim(leading FROM a), rtrim(a), extract(year FROM a), a AT TIME ZONE 'UTC', a AT LOCAL, collation for (a), normalize(a, NFKC), a IS NFC NORMALIZED, a IS NORMALIZED, (a, b) OVERLAPS (c, d);
SELECT count(*), count(DISTINCT a ORDER BY a), percentile_cont(0.5) WITHIN GROUP (ORDER BY a), sum(a) FILTER (WHERE a > 0) OVER (PARTITION BY b ORDER BY c ROWS BETWEEN 1 PRECEDING AND CURRENT ROW EXCLUDE TIES), f(VARIADIC ARRAY[1]), f(a => 1, b := 2), rank() OVER (w RANGE UNBOUNDED PRECEDING), lag(a) OVER (GROUPS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING EXCLUDE GROUP) FROM t WINDOW w AS (PARTITION BY a);
SELECT DISTINCT ON (a) a, b AS "B" FROM ONLY s.t AS x(a, b) LEFT JOIN u USING (a) AS j NATURAL FULL JOIN v CROSS JOIN LATERAL (SELECT 1) s(c) RIGHT JOIN (w JOIN y ON true) ON false, LATERAL f(1) WITH ORDINALITY AS g(a, b), ROWS FROM (f(1) AS (a int), g(2)) r, h() AS (a int COLLATE "C"), t2 TABLESAMPLE system(10) REPEATABLE (1) WHERE a > 1 GROUP BY DISTINCT ROLLUP (a), CUBE (b, c), GROUPING SETS ((a), ()), d HAVING count(*) > 1 ORDER BY a USING <, b DESC NULLS LAST LIMIT ALL OFFSET 2 FOR UPDATE OF t NOWAIT FOR KEY SHARE SKIP LOCKED;
SELECT 1 UNION SELECT 2 INTERSECT ALL (SELECT 3 ORDER BY 1) EXCEPT (SELECT 4 LIMIT 1) ORDER BY 1 FETCH FIRST 2 ROWS WITH TIES;
WITH RECURSIVE r(n) AS MATERIALIZED (SELECT 1 UNION ALL SELECT n + 1 FROM r) SEARCH DEPTH FIRST BY n SET ord CYCLE n SET is_cycle TO true DEFAULT false USING path, d AS NOT MATERIALIZED (DELETE FROM t RETURNING *) SELECT * FROM r;
VALUES (1, 'a'), (2, DEFAULT);
SELECT a INTO TEMPORARY TABLE n FROM t;
CREATE UNLOGGED TABLE IF NOT EXISTS c(a, b) USING heap WITH (fillfactor=70) ON COMMIT DELETE ROWS TABLESPACE ts AS SELECT 1, 2 WITH NO DATA;
CREATE MATERIALIZED VIEW mv AS SELECT 1;
INSERT INTO t AS x (a, b[1], c.d) OVERRIDING SYSTEM VALUE SELECT 1, 2, 3 ON CONFLICT (a COLLATE "C" text_pattern_ops DESC NULLS FIRST, (lower(b))) WHERE a > 0 DO UPDATE SET a = excluded.a, (b, c) = (SELECT 1, 2), d = DEFAULT WHERE x.a < 5 RETURNING WITH (OLD AS o, NEW AS n) o.a, n.*;
INSERT INTO t DEFAULT VALUES ON CONFLICT ON CONSTRAINT k DO NOTHING;
WITH s AS (SELECT 1) UPDATE ONLY t SET a = 1, b = s.x FROM s WHERE CURRENT OF c RETURNING a;
DELETE FROM t USING u, v WHERE t.a = u.a RETURNING *;
MERGE INTO t USING s ON t.a = s.a WHEN MATCHED AND s.b > 0 THEN UPDATE SET b = s.b WHEN MATCHED THEN DELETE WHEN NOT MATCHED BY SOURCE THEN DO NOTHING WHEN NOT MATCHED THEN INSERT (a, b) OVERRIDING USER VALUE VALUES (s.a, DEFAULT) WHEN NOT MATCHED BY TARGET THEN INSERT DEFAULT VALUES RETURNING merge_action(), t.*;
NOTIFY chan, 'it''s';
NOTIFY "Chan";
CREATE RULE r AS ON INSERT TO t WHERE new.a > 0 DO ALSO (NOTIFY t; INSERT INTO u VALUES (new.a); UPDATE v SET a = old.a);
CREATE VIEW v AS SELECT "select", "A b", "user", "left", "x""y", "int", "between", float8 FROM t;
CREATE TABLE p (a int DEFAULT nextval('s'::regclass) CHECK (a > 0 AND a < 10), b text GENERATED ALWAYS AS (upper(a::text)) STORED, CHECK (b <> ''), EXCLUDE USING gist (c WITH &&) WHERE (a > 1)) PARTITION BY RANGE (a, (lower(b)));
CREATE TABLE q PARTITION OF p FOR VALUES FROM (MINVALUE, 1) TO (10, MAXVALUE);
CREATE INDEX i ON t ((a + 1), lower(b) DESC, (c COLLATE "C")) WHERE a IS NOT NULL;
CREATE POLICY pol ON t USING (owner = current_user) WITH CHECK (a > 0);
CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW WHEN (old.* IS DISTINCT FROM new.*) EXECUTE FUNCTION f();
CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a + 1; INSERT INTO t VALUES (a); END;
CREATE FUNCTION g(a int) RETURNS int RETURN a * 2;
CREATE STATISTICS st ON (a + b), c FROM t;
CREATE PUBLICATION pub FOR TABLE t WHERE (a > 1 AND b IS NOT NULL);
PREPARE p1(int) AS SELECT $1 + 1;
COPY (SELECT 1 WHERE true) TO STDOUT;
ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0), ALTER COLUMN a SET DEFAULT 1 + 2, ALTER COLUMN b TYPE int USING b::int;
CREATE DOMAIN d AS int CHECK (VALUE > 0) DEFAULT 1;
"#;

/// Counts of the comparison.
#[derive(Default)]
struct Tally {
    statements: usize,
    expressions: usize,
    fallbacks: usize,
    /// Expressions neither deparser takes (libpg_query errors, so the native one defers).
    not_deparsable: usize,
    /// What was left to libpg_query (`DEPARSE_SHOW_FALLBACKS` prints it).
    fallback_texts: Vec<String>,
    mismatches: Vec<String>,
}

/// Where stored expressions sit in a statement (what the engine passes to `deparse_expr`),
/// and the statements nested in it (view queries, rule actions, function bodies, ...).
fn roots<'a>(n: &'a Node, stmts: &mut Vec<&'a Node>, exprs: &mut Vec<&'a Node>) {
    match &n.node {
        Some(
            E::SelectStmt(_)
            | E::InsertStmt(_)
            | E::UpdateStmt(_)
            | E::DeleteStmt(_)
            | E::MergeStmt(_)
            | E::NotifyStmt(_)
            | E::ExecuteStmt(_)
            | E::DeclareCursorStmt(_)
            | E::CreateTableAsStmt(_),
        ) => stmts.push(n),
        Some(E::CreateStmt(s)) => {
            for e in s.table_elts.iter().chain(&s.constraints) {
                constraint_roots(e, exprs);
            }
            if let Some(b) = &s.partbound {
                exprs.extend(b.listdatums.iter().chain(&b.lowerdatums).chain(&b.upperdatums));
            }
            if let Some(p) = &s.partspec {
                for e in &p.part_params {
                    if let Some(E::PartitionElem(pe)) = &e.node {
                        exprs.extend(pe.expr.as_deref());
                    }
                }
            }
        }
        Some(E::AlterTableStmt(s)) => {
            for c in &s.cmds {
                if let Some(E::AlterTableCmd(c)) = &c.node
                    && let Some(d) = c.def.as_deref()
                {
                    match &d.node {
                        Some(E::ColumnDef(_) | E::Constraint(_)) => constraint_roots(d, exprs),
                        Some(E::PartitionCmd(_)) => {}
                        _ => exprs.push(d),
                    }
                }
            }
        }
        Some(E::IndexStmt(s)) => {
            for e in &s.index_params {
                if let Some(E::IndexElem(ie)) = &e.node {
                    exprs.extend(ie.expr.as_deref());
                }
            }
            exprs.extend(s.where_clause.as_deref());
        }
        Some(E::CreatePolicyStmt(s)) => {
            exprs.extend(s.qual.as_deref());
            exprs.extend(s.with_check.as_deref());
        }
        Some(E::AlterPolicyStmt(s)) => {
            exprs.extend(s.qual.as_deref());
            exprs.extend(s.with_check.as_deref());
        }
        Some(E::CreateTrigStmt(s)) => exprs.extend(s.when_clause.as_deref()),
        Some(E::CreateDomainStmt(s)) => {
            for c in &s.constraints {
                constraint_roots(c, exprs);
            }
        }
        Some(E::AlterDomainStmt(s)) => {
            if let Some(d) = s.def.as_deref() {
                constraint_roots(d, exprs);
            }
        }
        Some(E::CreateFunctionStmt(s)) => {
            if let Some(b) = s.sql_body.as_deref() {
                body_roots(b, stmts, exprs);
            }
        }
        Some(E::RuleStmt(s)) => {
            exprs.extend(s.where_clause.as_deref());
            for a in &s.actions {
                roots(a, stmts, exprs);
            }
        }
        Some(E::ViewStmt(s)) => stmts.extend(s.query.as_deref()),
        Some(E::PrepareStmt(s)) => stmts.extend(s.query.as_deref()),
        Some(E::ExplainStmt(s)) => stmts.extend(s.query.as_deref()),
        Some(E::CopyStmt(s)) => {
            stmts.extend(s.query.as_deref());
            exprs.extend(s.where_clause.as_deref());
        }
        Some(E::CreateStatsStmt(s)) => {
            for e in &s.exprs {
                if let Some(E::StatsElem(se)) = &e.node {
                    exprs.extend(se.expr.as_deref());
                }
            }
        }
        Some(E::CreatePublicationStmt(s)) => pub_roots(&s.pubobjects, exprs),
        Some(E::AlterPublicationStmt(s)) => pub_roots(&s.pubobjects, exprs),
        _ => {}
    }
}

fn constraint_roots<'a>(n: &'a Node, exprs: &mut Vec<&'a Node>) {
    match &n.node {
        Some(E::ColumnDef(c)) => {
            exprs.extend(c.raw_default.as_deref());
            for k in &c.constraints {
                constraint_roots(k, exprs);
            }
        }
        Some(E::Constraint(c)) => {
            exprs.extend(c.raw_expr.as_deref());
            exprs.extend(c.where_clause.as_deref());
            for e in &c.exclusions {
                if let Some(E::List(l)) = &e.node
                    && let Some(Some(E::IndexElem(ie))) = l.items.first().map(|i| &i.node)
                {
                    exprs.extend(ie.expr.as_deref());
                }
            }
        }
        _ => {}
    }
}

fn body_roots<'a>(n: &'a Node, stmts: &mut Vec<&'a Node>, exprs: &mut Vec<&'a Node>) {
    match &n.node {
        Some(E::ReturnStmt(r)) => exprs.extend(r.returnval.as_deref()),
        Some(E::List(l)) => l.items.iter().for_each(|i| body_roots(i, stmts, exprs)),
        _ => roots(n, stmts, exprs),
    }
}

fn pub_roots<'a>(objs: &'a [Node], exprs: &mut Vec<&'a Node>) {
    for o in objs {
        if let Some(E::PublicationObjSpec(o)) = &o.node
            && let Some(t) = o.pubtable.as_deref()
        {
            exprs.extend(t.where_clause.as_deref());
        }
    }
}

fn text(r: pg_engine::Result<String>) -> String {
    r.unwrap_or_else(|e| format!("ERROR: {}", e.message))
}

impl Tally {
    fn statement(&mut self, n: &Node, at: &str) {
        self.statements += 1;
        let native = deparse::statement(n).is_ok();
        let (old, new) = (
            text(ast::deparse_stmt_libpg(n)),
            text(ast::deparse_stmt(n)),
        );
        if !native {
            self.fallbacks += 1;
            self.fallback_texts.push(format!("{at}: statement: {old}"));
        } else if old != new {
            self.mismatches.push(format!("{at}: statement\n  old: {old}\n  new: {new}"));
        }
        if let Ok(es) = deparse::expressions(n) {
            for e in es {
                self.expression(e, at);
            }
        }
    }

    fn expression(&mut self, n: &Node, at: &str) {
        self.expressions += 1;
        let native = deparse::expression(n).is_ok();
        let (old, new) = (
            text(ast::deparse_expr_libpg(n)),
            text(ast::deparse_expr(n)),
        );
        if !native && old.starts_with("ERROR: ") {
            // Not an expression libpg_query deparses either (a partition bound's MINVALUE,
            // an exclusion's operator list): the caller gets the same error.
            self.expressions -= 1;
            self.not_deparsable += 1;
        } else if !native {
            self.fallbacks += 1;
            self.fallback_texts.push(format!("{at}: expression: {old}"));
        } else if old != new {
            self.mismatches.push(format!("{at}: expression\n  old: {old}\n  new: {new}"));
        }
    }

    /// Every statement of a SQL file (psql commands and COPY data skipped).
    /// Returns 1 when the file doesn't parse whole.
    fn file(&mut self, name: &str, sql: &str) -> usize {
        let mut clean = String::with_capacity(sql.len());
        let mut copy_data = false;
        for line in sql.lines() {
            if copy_data {
                copy_data = line != "\\.";
                continue;
            }
            if line.trim_start().starts_with('\\') {
                continue;
            }
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("COPY ") && upper.contains("FROM STDIN") {
                copy_data = true;
            }
            clean.push_str(line);
            clean.push('\n');
        }
        // The whole file when it parses (the scanner's split cuts BEGIN ATOMIC bodies apart),
        // else statement by statement (regress files hold errors on purpose).
        let trees: Vec<(usize, pg_query::ParseResult)> = match pg_query::parse(&clean, 0) {
            Ok(t) => vec![(0, t)],
            Err(_) => pg_query::split_with_scanner(&clean)
                .unwrap_or_default()
                .into_iter()
                .filter(|p| !p.trim().is_empty())
                .enumerate()
                .filter_map(|(i, part)| match pg_query::parse(part, 0) {
                    Ok(t) => Some((i + 1, t)),
                    Err(e) => {
                        if name == "corpus" {
                            eprintln!("corpus doesn't parse: {e}: {part}");
                        }
                        None
                    }
                })
                .collect(),
        };
        let bad = usize::from(trees.is_empty() || trees[0].0 != 0);
        for (i, tree) in &trees {
            for (j, raw) in tree.protobuf.stmts.iter().enumerate() {
                let at = format!("{name}#{}", i + j + 1);
                let Some(n) = raw.stmt.as_deref() else {
                    continue;
                };
                let (mut stmts, mut exprs) = (Vec::new(), Vec::new());
                roots(n, &mut stmts, &mut exprs);
                for s in stmts {
                    self.statement(s, &at);
                }
                for e in exprs {
                    self.expression(e, &at);
                    if let Ok(inner) = deparse::expressions(&expr_stmt(e)) {
                        // The expressions nested in a stored one (skipping the root itself).
                        for x in inner.into_iter().skip(1) {
                            self.expression(x, &at);
                        }
                    }
                }
            }
        }
        bad
    }

    fn dir(&mut self, dir: &Path, prefix: &str) -> usize {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        files.retain(|p| p.extension().is_some_and(|e| e == "sql"));
        files.sort();
        for f in &files {
            let sql = String::from_utf8_lossy(&std::fs::read(f).unwrap()).into_owned();
            let name = format!("{prefix}{}", f.file_name().unwrap().to_string_lossy());
            self.file(&name, &sql);
        }
        files.len()
    }

    fn report(&self, what: &str) {
        eprintln!(
            "deparse identity, {what}: {} statements, {} expressions compared, {} mismatches, {} fallbacks ({} nodes libpg_query can't deparse either)",
            self.statements,
            self.expressions,
            self.mismatches.len(),
            self.fallbacks,
            self.not_deparsable
        );
        for m in self.mismatches.iter().take(40) {
            eprintln!("{m}");
        }
        if std::env::var_os("DEPARSE_SHOW_FALLBACKS").is_some() {
            for f in &self.fallback_texts {
                eprintln!("fallback {f}");
            }
        }
        assert!(self.mismatches.is_empty(), "{} mismatches", self.mismatches.len());
    }
}

/// `SELECT <expr>`, to collect the expressions nested in one.
fn expr_stmt(e: &Node) -> Node {
    use pg_query::protobuf as pb;
    let target = pb::ResTarget {
        val: Some(Box::new(e.clone())),
        location: -1,
        ..Default::default()
    };
    Node {
        node: Some(E::SelectStmt(Box::new(pb::SelectStmt {
            target_list: vec![Node {
                node: Some(E::ResTarget(Box::new(target))),
            }],
            op: 1,
            limit_option: 1,
            ..Default::default()
        }))),
    }
}

#[test]
fn goldens_and_corpus_deparse_as_before() {
    let mut t = Tally::default();
    assert_eq!(t.file("corpus", CORPUS), 0, "the corpus parses");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sql");
    let n = t.dir(&dir, "sql/");
    assert!(n > 100, "goldens not found in {}", dir.display());
    t.report("goldens and corpus");
    assert_eq!(t.fallbacks, 0, "statements or expressions left to libpg_query");
}

#[test]
fn regress_sql_deparses_as_before() {
    let dir = std::env::var_os("DEPARSE_REGRESS_SQL")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PGREGRESS_DIR")
                .map(|d| PathBuf::from(d).join("src-repo/src/test/regress/sql"))
        });
    let Some(dir) = dir.filter(|d| d.is_dir()) else {
        eprintln!("deparse identity: no regress SQL (set DEPARSE_REGRESS_SQL), skipped");
        return;
    };
    let mut t = Tally::default();
    t.dir(&dir, "regress/");
    t.report("PostgreSQL regress SQL");
}

/// A 4,875-term chain (the engine's depth limit) deparses natively on a 2 MB thread.
#[test]
fn deep_chain_deparses_on_a_small_thread() {
    use pg_query::protobuf as pb;
    const N: usize = 4875;
    let name = |s: &'static str| Node {
        node: Some(E::String(pb::String { sval: s.into() })),
    };
    let col = move || Node {
        node: Some(E::ColumnRef(pb::ColumnRef {
            fields: vec![name("x")],
            location: -1,
        })),
    };
    let h = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let mut e = col();
            for _ in 1..N {
                e = Node {
                    node: Some(E::AExpr(Box::new(pb::AExpr {
                        kind: pb::AExprKind::AexprOp as i32,
                        name: vec![name("+")],
                        lexpr: Some(Box::new(e)),
                        rexpr: Some(Box::new(col())),
                        location: -1,
                        ..Default::default()
                    }))),
                };
            }
            let out = deparse::expression(&e).unwrap();
            assert_eq!(out, vec!["x"; N].join(" + "));
            pg_engine::stack::with_headroom(256 << 20, || drop(e));
        })
        .unwrap();
    h.join().unwrap();
}
