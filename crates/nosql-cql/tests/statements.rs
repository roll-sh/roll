//! The syntax tree of every statement form, and the printer: every statement of the corpora
//! prints back as CQL that parses to the same tree (and prints the same again).

use std::borrow::Cow;

use nosql_cql::*;

const COMMON: &str = include_str!("data/common.cql");
const CASSANDRA: &str = include_str!("data/cassandra.cql");
const SCYLLA: &str = include_str!("data/scylla.cql");

fn lines(s: &str) -> impl Iterator<Item = String> + '_ {
    s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.replace("\\n", "\n"))
}

fn one(src: &str, d: Dialect) -> Statement<'_> {
    parse_statement(src, d).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn c(src: &str) -> StatementKind<'_> {
    one(src, Dialect::Cassandra).kind
}

fn s(src: &str) -> StatementKind<'_> {
    one(src, Dialect::Scylla).kind
}

fn err(src: &str, d: Dialect) -> String {
    match parse_statement(src, d) {
        Ok(st) => panic!("{src}: parsed as {st:?}"),
        Err(e) => e.message,
    }
}

fn round_trip(src: &str, d: Dialect) {
    let first = one(src, d);
    let printed = first.to_string();
    let second = parse_statement(&printed, d).unwrap_or_else(|e| panic!("{src}\n  printed: {printed}\n  error: {e}"));
    assert_eq!(first.kind, second.kind, "{src}\n  printed: {printed}");
    assert_eq!(first.bind_markers, second.bind_markers, "{src}");
    assert_eq!(printed, second.to_string(), "printing is not stable for {src}");
}

#[test]
fn every_statement_form_parses_and_round_trips() {
    let mut n = 0;
    for l in lines(COMMON) {
        round_trip(&l, Dialect::Cassandra);
        round_trip(&l, Dialect::Scylla);
        n += 2;
    }
    for l in lines(CASSANDRA) {
        round_trip(&l, Dialect::Cassandra);
        n += 1;
    }
    for l in lines(SCYLLA) {
        round_trip(&l, Dialect::Scylla);
        n += 1;
    }
    // Every statement a reference accepts (including the mutations of the corpora it accepts).
    for (corpus, gaps, d) in [
        (include_str!("data/cassandra_reference.tsv"), include_str!("data/cassandra_known_gaps.txt"), Dialect::Cassandra),
        (include_str!("data/scylla_reference.tsv"), include_str!("data/scylla_known_gaps.txt"), Dialect::Scylla),
    ] {
        for line in corpus.lines().filter(|l| !l.starts_with('#')) {
            if let Some((q, "OK")) = line.split_once('\t')
                && !gaps.lines().any(|g| g == q)
            {
                round_trip(&q.replace("\\n", "\n").replace("\\t", "\t").replace("\\r", "\r"), d);
                n += 1;
            }
        }
    }
    assert!(n > 1200, "{n}");
}

#[test]
fn dialect_specific_syntax_is_rejected_by_the_other_dialect() {
    for src in [
        "SELECT * FROM t BYPASS CACHE",
        "SELECT * FROM t USING TIMEOUT 1s",
        "INSERT INTO t (k) VALUES (1) USING TIMEOUT 1s",
        "PRUNE MATERIALIZED VIEW mv",
        "CREATE SERVICE LEVEL sl",
        "LIST ALL SERVICE LEVELS",
        "ATTACH SERVICE LEVEL sl TO r",
        "SELECT * FROM t WHERE k NOT IN (1)",
        "DELETE FROM t WHERE k = 1 BYPASS LARGE_DATA_GUARDRAILS",
        "ALTER TABLE t TTL c",
        "CREATE TABLE t (k int PRIMARY KEY, e timestamp TTL)",
        "CREATE INDEX ON t ((k), v)",
        "CREATE AGGREGATE a(int) SFUNC f STYPE int REDUCEFUNC r",
        "DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS",
    ] {
        parse_statement(src, Dialect::Scylla).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert!(parse_statement(src, Dialect::Cassandra).is_err(), "{src}");
    }
    for src in [
        "SELECT a + 1 FROM t",
        "SELECT l[1..2] FROM t",
        "SELECT (int) a FROM t",
        "SELECT * FROM t WHERE k = 1 + 1",
        "SELECT * FROM t WHERE expr(i, 'q')",
        "UPDATE t SET c += 1 WHERE k = 1",
        "CREATE TABLE t (k int PRIMARY KEY, v int MASKED WITH DEFAULT)",
        "ALTER TABLE IF EXISTS t ADD v int",
        "ALTER TABLE t DROP COMPACT STORAGE",
        "CREATE TRIGGER x ON t USING 'c'",
        "ADD IDENTITY 'a' TO ROLE 'b'",
        "CREATE ROLE r WITH ACCESS TO ALL DATACENTERS",
        "GRANT UNMASK ON TABLE t TO r",
        "GRANT SELECT, MODIFY ON KEYSPACE ks TO r",
        "CREATE TABLE t (k 'UTF8Type' PRIMARY KEY)",
        "SELECT * FROM t WHERE \"\" = 1",
    ] {
        parse_statement(src, Dialect::Cassandra).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert!(parse_statement(src, Dialect::Scylla).is_err(), "{src}");
    }
}

fn ident(t: &str) -> Ident<'_> {
    Ident::unquoted(t)
}

fn int(t: &str) -> Term<'_> {
    Term::Literal(Literal { kind: LiteralKind::Integer, text: Cow::Borrowed(t), dollar: false })
}

fn string(t: &str) -> Term<'_> {
    Term::Literal(Literal { kind: LiteralKind::String, text: Cow::Borrowed(t), dollar: false })
}

fn qn<'a>(ks: Option<&'a str>, n: &'a str) -> QualifiedName<'a> {
    QualifiedName { keyspace: ks.map(ident), name: ident(n) }
}

fn select(k: StatementKind<'_>) -> Select<'_> {
    match k {
        StatementKind::Select(s) => *s,
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn select_clauses() {
    let sel = select(c("SELECT JSON DISTINCT a AS x, \"B\" FROM ks.t WHERE k = 1 AND c > ? GROUP BY k ORDER BY c DESC \
                         PER PARTITION LIMIT 2 LIMIT :l ALLOW FILTERING"));
    assert!(sel.json && sel.distinct && sel.allow_filtering && !sel.bypass_cache);
    assert_eq!(sel.table, qn(Some("ks"), "t"));
    assert_eq!(sel.selectors.len(), 2);
    assert_eq!(sel.selectors[0], Selector { expr: Selectable::Column(ident("a")), alias: Some(ident("x")) });
    assert_eq!(sel.selectors[1].expr, Selectable::Column(Ident::quoted("B")));
    assert_eq!(sel.where_clause.len(), 2);
    assert_eq!(
        sel.where_clause[1],
        Relation::Single(SingleRelation {
            column: ident("c"),
            key: None,
            op: Operator::Gt,
            value: Value::Term(Term::Bind(BindMarker { index: 0, name: None })),
        })
    );
    assert_eq!(sel.group_by, vec![Selectable::Column(ident("k"))]);
    assert_eq!(sel.order_by, vec![OrderBy { expr: OrderByExpr::Column(ident("c")), direction: Some(Direction::Desc) }]);
    assert_eq!(sel.per_partition_limit, Some(int("2")));
    assert_eq!(sel.limit, Some(Term::Bind(BindMarker { index: 1, name: Some(ident("l")) })));

    let sel = select(c("SELECT * FROM t"));
    assert!(sel.selectors.is_empty() && !sel.json && !sel.distinct);

    // `json` and `distinct` are also column names.
    let sel = select(c("SELECT json, distinct FROM t"));
    assert!(!sel.json && !sel.distinct);
    assert_eq!(sel.selectors[0].expr, Selectable::Column(ident("json")));

    let sel = select(s("SELECT * FROM t WHERE k = 1 ALLOW FILTERING BYPASS CACHE USING TIMEOUT 10ms"));
    assert!(sel.bypass_cache);
    assert_eq!(sel.using.order, vec![UsingKind::Timeout]);
    assert_eq!(
        sel.using.timeout,
        Some(Term::Literal(Literal { kind: LiteralKind::Duration, text: Cow::Borrowed("10ms"), dollar: false }))
    );

    let sel = select(c("SELECT * FROM t ORDER BY v ANN OF [1.0, 2.0] LIMIT 5"));
    match &sel.order_by[0].expr {
        OrderByExpr::Ann(c, Term::List(v)) => assert_eq!((c.text.as_ref(), v.len()), ("v", 2)),
        other => panic!("{other:?}"),
    }
    let sel = select(s("SELECT * FROM t WHERE BM25(v, 'q') > 0 ORDER BY BM25(v, 'q')"));
    assert!(matches!(sel.where_clause[0], Relation::Function(_)));
    assert!(matches!(sel.order_by[0].expr, OrderByExpr::Function(..)));
}

#[test]
fn selectors() {
    let sel = select(c("SELECT COUNT(*), count(1), WRITETIME(v), TTL(m['a']), MAXWRITETIME(u.f), CAST(v AS text), \
                        ks.f(a, 1), token(k), m['a'], l[1..2], l[..2], u.f.g, (int) a, a + b * c, -a, [a], {a}, \
                        {a: b}, (a, b), (a), 'x', NULL, ? FROM t"));
    let e: Vec<_> = sel.selectors.into_iter().map(|s| s.expr).collect();
    assert_eq!(e[0], Selectable::CountRows);
    assert!(matches!(&e[1], Selectable::Function(n, a) if n.name.text == "count" && a.len() == 1));
    assert_eq!(e[2], Selectable::Metadata(MetadataKind::WriteTime, Box::new(Selectable::Column(ident("v")))));
    assert!(
        matches!(&e[3], Selectable::Metadata(MetadataKind::Ttl, a) if matches!(**a, Selectable::Element(_, Term::Literal(_))))
    );
    assert!(
        matches!(&e[4], Selectable::Metadata(MetadataKind::MaxWriteTime, a) if matches!(**a, Selectable::Field(..)))
    );
    assert!(matches!(&e[5], Selectable::Cast(c) if c.ty == NativeType::Text));
    assert!(matches!(&e[6], Selectable::Function(n, a) if n.keyspace == Some(ident("ks")) && a.len() == 2));
    assert!(matches!(&e[7], Selectable::Function(n, _) if n.name.text == "token"));
    assert!(matches!(&e[8], Selectable::Element(..)));
    assert!(matches!(&e[9], Selectable::Slice(s) if s.from.is_some() && s.to.is_some()));
    assert!(matches!(&e[10], Selectable::Slice(s) if s.from.is_none() && s.to.is_some()));
    assert!(matches!(&e[11], Selectable::Field(inner, f) if f.text == "g" && matches!(**inner, Selectable::Field(..))));
    assert!(matches!(&e[12], Selectable::TypeHint(RawType::Native(NativeType::Int), _)));
    match &e[13] {
        Selectable::Binary(b) => {
            assert_eq!(b.op, ArithOp::Add);
            assert!(matches!(&b.right, Selectable::Binary(m) if m.op == ArithOp::Mul));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(&e[14], Selectable::Negate(_)));
    assert!(matches!(&e[15], Selectable::List(l) if l.len() == 1));
    assert!(matches!(&e[16], Selectable::Set(l) if l.len() == 1));
    assert!(matches!(&e[17], Selectable::MapOrUdt(l) if l.len() == 1));
    assert!(matches!(&e[18], Selectable::Tuple(l) if l.len() == 2));
    assert!(matches!(&e[19], Selectable::Tuple(l) if l.len() == 1));
    assert_eq!(e[20], Selectable::Term(string("x")));
    assert_eq!(e[21], Selectable::Term(Term::Null));
    assert_eq!(e[22], Selectable::Term(Term::Bind(BindMarker { index: 0, name: None })));
}

#[test]
fn relations() {
    let sel = select(c("SELECT * FROM t WHERE k IN (1, 2) AND k IN ? AND m CONTAINS 1 AND m CONTAINS KEY 'a' \
                        AND m['a'] = 1 AND token(a, b) > token(1, 2) AND (c1, c2) >= (1, 2) AND (c1, c2) IN ((1, 2)) \
                        AND (c1, c2) IN ? AND (c1, c2) IN (?, :x) AND (c1, c2) < ? AND v IS NOT NULL AND v LIKE 'a%' \
                        AND (k = 1) AND expr(idx, 'q') AND v != 1"));
    let w = sel.where_clause;
    assert!(matches!(&w[0], Relation::Single(r) if r.op == Operator::In && matches!(&r.value, Value::List(l) if l.len() == 2)));
    assert!(matches!(&w[1], Relation::Single(r) if r.op == Operator::In && matches!(r.value, Value::ListMarker(_))));
    assert!(matches!(&w[2], Relation::Single(r) if r.op == Operator::Contains));
    assert!(matches!(&w[3], Relation::Single(r) if r.op == Operator::ContainsKey));
    assert!(matches!(&w[4], Relation::Single(r) if r.key.is_some() && r.op == Operator::Eq));
    assert!(matches!(&w[5], Relation::Token(r) if r.columns.len() == 2 && matches!(r.value, Term::Function(_))));
    assert!(matches!(&w[6], Relation::MultiColumn(r) if r.op == Operator::Gte && matches!(r.value, Value::Term(Term::Tuple(_)))));
    assert!(matches!(&w[7], Relation::MultiColumn(r) if r.op == Operator::In && matches!(&r.value, Value::List(l) if l.len() == 1)));
    assert!(matches!(&w[8], Relation::MultiColumn(r) if matches!(r.value, Value::ListMarker(_))));
    assert!(matches!(&w[9], Relation::MultiColumn(r) if matches!(&r.value, Value::List(l) if l.len() == 2)));
    assert!(matches!(&w[10], Relation::MultiColumn(r) if matches!(r.value, Value::Term(Term::Bind(_)))));
    assert!(matches!(&w[11], Relation::Single(r) if r.op == Operator::IsNot && r.value == Value::Term(Term::Null)));
    assert!(matches!(&w[12], Relation::Single(r) if r.op == Operator::Like));
    assert!(matches!(&w[13], Relation::Single(r) if r.column.text == "k"));
    assert!(matches!(&w[14], Relation::Expression(e) if e.index.name.text == "idx"));
    assert!(matches!(&w[15], Relation::Single(r) if r.op == Operator::Neq));

    let sel = select(s("SELECT * FROM t WHERE k NOT IN (1) AND c NOT IN ?"));
    assert!(sel.where_clause.iter().all(|r| matches!(r, Relation::Single(r) if r.op == Operator::NotIn)));
}

#[test]
fn terms_and_literals() {
    let sel = select(c("SELECT * FROM t WHERE a = -1 AND b = 1.5e3 AND c = NaN AND d = -Infinity AND e = true \
                        AND f = 0xCAFE AND g = 550e8400-e29b-41d4-a716-446655440000 AND h = P1D AND i = $$x'y$$ \
                        AND j = 'it''s' AND k = {x: 1} AND l = {'a': 1} AND m = {1, 2} AND n = [] AND o = {} \
                        AND p = (1, 'a') AND q = (int) 1 AND r = CAST(1 AS bigint) AND s = 1 + 2 * 3 AND u = - 1 \
                        AND v = f() AND w = ks.f(1, ?)"));
    let v: Vec<_> = sel
        .where_clause
        .into_iter()
        .map(|r| match r {
            Relation::Single(r) => match r.value {
                Value::Term(t) => t,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    let lit = |t: &Term<'_>| match t {
        Term::Literal(l) => (l.kind, l.text.to_string()),
        other => panic!("{other:?}"),
    };
    assert_eq!(lit(&v[0]), (LiteralKind::Integer, "-1".into()));
    assert_eq!(lit(&v[1]), (LiteralKind::Float, "1.5e3".into()));
    assert_eq!(lit(&v[2]), (LiteralKind::Float, "NaN".into()));
    assert_eq!(lit(&v[3]), (LiteralKind::Float, "-Infinity".into()));
    assert_eq!(lit(&v[4]), (LiteralKind::Boolean, "true".into()));
    assert_eq!(lit(&v[5]), (LiteralKind::Hex, "0xCAFE".into()));
    assert_eq!(lit(&v[6]), (LiteralKind::Uuid, "550e8400-e29b-41d4-a716-446655440000".into()));
    assert_eq!(lit(&v[7]), (LiteralKind::Duration, "P1D".into()));
    assert_eq!(lit(&v[8]), (LiteralKind::String, "x'y".into()));
    assert!(matches!(&v[8], Term::Literal(l) if l.dollar));
    assert_eq!(lit(&v[9]), (LiteralKind::String, "it's".into()));
    assert!(matches!(&v[10], Term::Udt(f) if f[0].0.text == "x"));
    assert!(matches!(&v[11], Term::Map(m) if m.len() == 1));
    assert!(matches!(&v[12], Term::Set(m) if m.len() == 2));
    assert_eq!(v[13], Term::List(vec![]));
    assert_eq!(v[14], Term::Set(vec![]));
    assert!(matches!(&v[15], Term::Tuple(t) if t.len() == 2));
    assert!(matches!(&v[16], Term::TypeHint(h) if h.ty == RawType::Native(NativeType::Int)));
    assert!(matches!(&v[17], Term::Cast(c) if c.ty == NativeType::Bigint));
    assert!(matches!(&v[18], Term::Binary(b) if b.op == ArithOp::Add && matches!(&b.right, Term::Binary(m) if m.op == ArithOp::Mul)));
    assert!(matches!(&v[19], Term::Negate(_)));
    assert!(matches!(&v[20], Term::Function(f) if f.args.is_empty()));
    assert!(matches!(&v[21], Term::Function(f) if f.name.keyspace.is_some() && f.args.len() == 2));
}

#[test]
fn insert_update_delete_batch() {
    match c("INSERT INTO ks.t (k, v) VALUES (1, ?) IF NOT EXISTS USING TTL 10 AND TIMESTAMP :ts") {
        StatementKind::Insert(i) => {
            assert_eq!(i.table, qn(Some("ks"), "t"));
            assert!(i.if_not_exists);
            assert_eq!(i.using.order, vec![UsingKind::Ttl, UsingKind::Timestamp]);
            assert_eq!(i.using.ttl, Some(int("10")));
            assert!(matches!(&i.values, InsertValues::Values { columns, values } if columns.len() == 2 && values.len() == 2));
        }
        other => panic!("{other:?}"),
    }
    match c("INSERT INTO t JSON '{}' DEFAULT UNSET") {
        StatementKind::Insert(i) => {
            assert!(matches!(i.values, InsertValues::Json { default: Some(JsonDefault::Unset), .. }))
        }
        other => panic!("{other:?}"),
    }
    match c("UPDATE t USING TTL 1 SET a = 1, b = b + 1, c = c - {1}, d = [0] + d, m['k'] = 1, u.f = 2, n = n -3, \
             p += 1, q -= 1 WHERE k = 1 IF a = 1 AND m['k'] IN (1, 2) AND u.f > 1")
    {
        StatementKind::Update(u) => {
            let ops: Vec<_> = u.assignments.iter().map(|a| &a.op).collect();
            assert!(matches!(ops[0], Operation::Set(_)));
            assert!(matches!(ops[1], Operation::Add(_, OpForm::Long)));
            assert!(matches!(ops[2], Operation::Subtract(_, OpForm::Long)));
            assert!(matches!(ops[3], Operation::Prepend(_)));
            assert!(matches!(ops[4], Operation::SetElement(..)));
            assert!(matches!(ops[5], Operation::SetField(..)));
            assert!(matches!(ops[6], Operation::Add(t, OpForm::Juxtaposed) if *t == int("-3")));
            assert!(matches!(ops[7], Operation::Add(_, OpForm::Shorthand)));
            assert!(matches!(ops[8], Operation::Subtract(_, OpForm::Shorthand)));
            match &u.conditions {
                Conditions::List(c) => {
                    assert_eq!(c.len(), 3);
                    assert!(matches!(c[1].target, ConditionTarget::Element(_)) && c[1].op == Operator::In);
                    assert!(matches!(c[2].target, ConditionTarget::Field(_)));
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
    match c("DELETE a, m['k'], u.f FROM t USING TIMESTAMP 1 WHERE k = 1 IF EXISTS") {
        StatementKind::Delete(d) => {
            assert_eq!(d.columns.len(), 3);
            assert!(matches!(d.columns[1], Deletion::Element(..)) && matches!(d.columns[2], Deletion::Field(..)));
            assert_eq!(d.conditions, Conditions::Exists);
            assert_eq!(d.using.timestamp, Some(int("1")));
        }
        other => panic!("{other:?}"),
    }
    match s("DELETE FROM t WHERE k = 1 IF EXISTS BYPASS LARGE_DATA_GUARDRAILS") {
        StatementKind::Delete(d) => assert!(d.bypass_guardrails),
        other => panic!("{other:?}"),
    }
    match c("BEGIN UNLOGGED BATCH USING TIMESTAMP 1 INSERT INTO t (k) VALUES (1); UPDATE t SET v = 1 WHERE k = 1 \
             DELETE FROM t WHERE k = 2; APPLY BATCH")
    {
        StatementKind::Batch(b) => {
            assert_eq!(b.kind, BatchKind::Unlogged);
            assert_eq!(b.statements.len(), 3);
            assert!(matches!(b.statements[2], BatchStatement::Delete(_)));
        }
        other => panic!("{other:?}"),
    }
    // A column named `exists` in a condition, and `IF EXISTS`.
    match c("UPDATE t SET v = 1 WHERE k = 1 IF exists = 1") {
        StatementKind::Update(u) => assert!(matches!(u.conditions, Conditions::List(_))),
        other => panic!("{other:?}"),
    }
}

#[test]
fn ddl() {
    match c("CREATE TABLE IF NOT EXISTS ks.t (a int, b text STATIC, c frozen<map<text, list<int>>>, d tuple<int, \
             vector<float, 3>>, e ks.udt, PRIMARY KEY ((a, b), c)) WITH CLUSTERING ORDER BY (c DESC) AND comment = 'x' \
             AND compaction = {'class': 'LCS'} AND COMPACT STORAGE")
    {
        StatementKind::CreateTable(t) => {
            assert!(t.if_not_exists);
            assert_eq!(t.columns.len(), 5);
            assert!(t.columns[1].is_static);
            assert!(matches!(&t.columns[2].ty, RawType::Frozen(m) if matches!(**m, RawType::Map(_))));
            assert!(matches!(&t.columns[3].ty, RawType::Tuple(v) if matches!(&v[1], RawType::Vector(b) if b.1 == "3")));
            assert!(matches!(&t.columns[4].ty, RawType::User(n) if n.keyspace.is_some()));
            assert_eq!(t.primary_keys.len(), 1);
            assert!(t.primary_keys[0].composite);
            assert_eq!(t.primary_keys[0].partition.len(), 2);
            assert_eq!(t.options.len(), 4);
            assert!(matches!(&t.options[0], TableOption::ClusteringOrder(o) if o[0].1 == Direction::Desc));
            assert!(matches!(&t.options[2], TableOption::Property(p) if matches!(p.value, PropertyValue::Map(_))));
            assert_eq!(t.options[3], TableOption::CompactStorage);
        }
        other => panic!("{other:?}"),
    }
    match c("CREATE TABLE t (k int PRIMARY KEY, v text MASKED WITH mask_inner(1, null))") {
        StatementKind::CreateTable(t) => {
            assert!(t.columns[0].primary_key);
            assert!(matches!(&t.columns[1].mask, Some(m) if m.args.len() == 2));
        }
        other => panic!("{other:?}"),
    }
    match s("CREATE TABLE t (k int PRIMARY KEY, e timestamp TTL)") {
        StatementKind::CreateTable(t) => assert!(t.columns[1].ttl),
        other => panic!("{other:?}"),
    }
    // `USING SERVICE LEVEL` (ScyllaDB SELECT), with or before TIMEOUT.
    for (src, name) in [
        ("SELECT * FROM t USING SERVICE LEVEL sl", "sl"),
        ("SELECT * FROM t USING TIMEOUT 1s AND SERVICE LEVEL \"Sl\"", "Sl"),
        ("SELECT * FROM t ALLOW FILTERING USING SERVICE LEVEL 'x' AND TIMEOUT 1s", "x"),
        ("SELECT * FROM t USING SERVICE_LEVEL default", "default"),
    ] {
        let sel = select(s(src));
        assert_eq!(sel.using.service_level.as_ref().map(|n| n.name().into_owned()).as_deref(), Some(name), "{src}");
        round_trip(src, Dialect::Scylla);
    }
    assert!(parse_statement("INSERT INTO t (a) VALUES (1) USING SERVICE LEVEL sl", Dialect::Scylla).is_err());
    assert!(parse_statement("SELECT * FROM t USING SERVICE LEVEL sl", Dialect::Cassandra).is_err());
    // `FROM MUTATION_FRAGMENTS(t)` (ScyllaDB); `mutation_fragments` stays a valid table name.
    for (src, mf, table) in [
        ("SELECT * FROM MUTATION_FRAGMENTS(ks.t) WHERE pk = 1", true, "t"),
        ("SELECT * FROM mutation_fragments (t)", true, "t"),
        ("SELECT * FROM mutation_fragments", false, "mutation_fragments"),
        ("SELECT * FROM ks.mutation_fragments", false, "mutation_fragments"),
    ] {
        match s(src) {
            StatementKind::Select(sel) => {
                assert_eq!((sel.mutation_fragments, sel.table.name.name().as_ref()), (mf, table), "{src}");
                round_trip(src, Dialect::Scylla);
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(parse_statement("SELECT * FROM mutation_fragments(t)", Dialect::Cassandra).is_err());
    assert!(parse_statement("SELECT * FROM MUTATION_FRAGMENTS(t", Dialect::Scylla).is_err());
    // `TTL` precedes `STATIC` (the analyzer rejects a static TTL column).
    match s("CREATE TABLE t (k int, c int, e bigint TTL STATIC, PRIMARY KEY (k, c))") {
        StatementKind::CreateTable(t) => assert!(t.columns[2].ttl && t.columns[2].is_static),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        err("CREATE TABLE t (k int, c int, e bigint STATIC TTL, PRIMARY KEY (k, c))", Dialect::Scylla),
        "line 1:46 : Missing ')'"
    );
    let alter = |src| match c(src) {
        StatementKind::AlterTable(a) => a.action,
        other => panic!("{other:?}"),
    };
    assert!(matches!(alter("ALTER TABLE t ADD (a int, b int STATIC)"), AlterTableAction::Add { columns, parenthesized: true, .. } if columns.len() == 2));
    assert!(matches!(alter("ALTER TABLE t DROP a USING TIMESTAMP 5"), AlterTableAction::DropObject { timestamp: Some(_), .. }));
    assert!(matches!(alter("ALTER TABLE t RENAME a TO b AND c TO d"), AlterTableAction::Rename { renames, .. } if renames.len() == 2));
    assert!(matches!(alter("ALTER TABLE t ALTER a TYPE text"), AlterTableAction::AlterType { .. }));
    assert!(matches!(alter("ALTER TABLE t ALTER a DROP MASKED"), AlterTableAction::Mask { mask: None, .. }));
    assert!(matches!(alter("ALTER TABLE t WITH comment = 'x'"), AlterTableAction::With(p) if p.len() == 1));
    assert!(matches!(alter("ALTER TABLE t DROP COMPACT STORAGE"), AlterTableAction::DropCompactStorage));
    match s("ALTER TABLE t TTL NULL") {
        StatementKind::AlterTable(a) => assert_eq!(a.action, AlterTableAction::Ttl(None)),
        other => panic!("{other:?}"),
    }
    match c("CREATE CUSTOM INDEX IF NOT EXISTS i ON ks.t (keys(m), entries(n), values(l), full(f), v) USING 'sai' \
             WITH OPTIONS = {'a': 'b'}")
    {
        StatementKind::CreateIndex(i) => {
            assert!(i.custom && i.if_not_exists);
            assert_eq!(i.targets.len(), 5);
            assert_eq!(i.targets[0], IndexTarget::Column(IndexTargetKind::Keys, ident("m")));
            assert_eq!(i.targets[4], IndexTarget::Column(IndexTargetKind::Simple, ident("v")));
            assert_eq!(i.using.as_deref(), Some("sai"));
        }
        other => panic!("{other:?}"),
    }
    match s("CREATE INDEX ON t ((a, b), c)") {
        StatementKind::CreateIndex(i) => assert!(matches!(&i.targets[0], IndexTarget::PartitionKey(p) if p.len() == 2)),
        other => panic!("{other:?}"),
    }
    match c("CREATE MATERIALIZED VIEW mv AS SELECT a, b FROM t WHERE a IS NOT NULL PRIMARY KEY (a, b) WITH comment = ''")
    {
        StatementKind::CreateView(v) => {
            assert_eq!(v.selectors.len(), 2);
            assert_eq!(v.primary_key.clustering, vec![ident("b")]);
        }
        other => panic!("{other:?}"),
    }
    match c("CREATE OR REPLACE FUNCTION IF NOT EXISTS ks.f(a int, b list<int>) CALLED ON NULL INPUT RETURNS int \
             LANGUAGE JAVA AS $$ return a; $$")
    {
        StatementKind::CreateFunction(f) => {
            assert!(f.or_replace && f.if_not_exists && f.called_on_null_input);
            assert_eq!(f.language, "java");
            assert_eq!(f.body.text, " return a; ");
            assert_eq!(f.args.len(), 2);
        }
        other => panic!("{other:?}"),
    }
    // ScyllaDB's service level `default` (its statements are answered by the engine).
    for q in [
        "CREATE SERVICE LEVEL default",
        "ATTACH SERVICE LEVEL default TO r",
        "ALTER SERVICE LEVEL default WITH shares = 5",
        "DROP SERVICE LEVEL default",
        "LIST SERVICE LEVEL default",
    ] {
        s(q);
    }
    // A keyspace-qualified function named `token` or `count` is a function call.
    for d in [Dialect::Scylla, Dialect::Cassandra] {
        match one("SELECT ks.token(v), ks.count(v) FROM t", d).kind {
            StatementKind::Select(sel) => assert_eq!(sel.selectors.len(), 2),
            other => panic!("{other:?}"),
        }
    }
    // ScyllaDB's VALUES take `- Infinity` / `- NaN`, and nothing else after a lone `-`.
    match s("INSERT INTO t (a, b) VALUES (1, -Infinity)") {
        StatementKind::Insert(i) => match &i.values {
            InsertValues::Values { values, .. } => {
                assert!(matches!(&values[1], Term::Literal(l) if l.text == "-Infinity"))
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    assert_eq!(err("INSERT INTO t (a, b) VALUES (1, -x)", Dialect::Scylla), "line 1:33 : Unexpected 'x'");
    // ScyllaDB's grammar rejects `OR REPLACE` with `IF NOT EXISTS` for functions (not aggregates).
    assert_eq!(
        err("CREATE OR REPLACE FUNCTION IF NOT EXISTS ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x'", Dialect::Scylla),
        "line 1:27 no viable alternative at input 'IF'"
    );
    assert!(matches!(
        s("CREATE OR REPLACE AGGREGATE IF NOT EXISTS ks.a(int) SFUNC f STYPE int"),
        StatementKind::CreateAggregate(a) if a.or_replace && a.if_not_exists
    ));
    match s("CREATE AGGREGATE a(int) SFUNC sf STYPE int REDUCEFUNC rf FINALFUNC ff INITCOND 0") {
        StatementKind::CreateAggregate(a) => {
            assert!(a.reduce_function.is_some() && a.final_function.is_some() && a.initial_condition.is_some())
        }
        other => panic!("{other:?}"),
    }
    match c("DROP FUNCTION IF EXISTS ks.f(int, text)") {
        StatementKind::DropFunction(d) => assert_eq!(d.arg_types.as_ref().map(Vec::len), Some(2)),
        other => panic!("{other:?}"),
    }
    match c("DROP FUNCTION f") {
        StatementKind::DropFunction(d) => assert!(d.arg_types.is_none()),
        other => panic!("{other:?}"),
    }
    match c("CREATE KEYSPACE ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1} AND durable_writes = false")
    {
        StatementKind::CreateKeyspace(k) => {
            assert_eq!(k.properties.len(), 2);
            assert!(matches!(&k.properties[0].value, PropertyValue::Map(m) if m.len() == 2));
            assert!(matches!(&k.properties[1].value, PropertyValue::Simple(l) if l.kind == LiteralKind::Boolean));
        }
        other => panic!("{other:?}"),
    }
    match c("ALTER TYPE IF EXISTS ty ADD IF NOT EXISTS f int") {
        StatementKind::AlterType(t) => {
            assert!(t.if_exists);
            assert!(matches!(t.action, AlterTypeAction::Add { if_not_exists: true, .. }));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(c("TRUNCATE TABLE ks.t"), StatementKind::Truncate(_)));
    assert_eq!(c("USE \"Ks\""), StatementKind::Use(Ident::quoted("Ks")));
    match s("PRUNE MATERIALIZED VIEW mv WHERE v = 1 USING CONCURRENCY 10 AND TIMEOUT 1s") {
        StatementKind::PruneView(p) => {
            assert_eq!(p.where_clause.len(), 1);
            assert_eq!(p.using.order, vec![UsingKind::Concurrency, UsingKind::Timeout]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn dcl_and_describe() {
    match c("GRANT SELECT PERMISSION, MODIFY ON ALL TABLES IN KEYSPACE ks TO 'R'") {
        StatementKind::Grant(g) => {
            assert_eq!(g.permissions, Permissions::List(vec![(Permission::Select, true), (Permission::Modify, false)]));
            assert_eq!(g.resource, Resource::AllTablesInKeyspace(ident("ks")));
            assert_eq!(g.role, RoleName { text: Cow::Borrowed("R"), form: NameForm::String });
        }
        other => panic!("{other:?}"),
    }
    let res = |src| match c(src) {
        StatementKind::Grant(g) => g.resource,
        other => panic!("{other:?}"),
    };
    assert_eq!(res("GRANT ALL ON ALL KEYSPACES TO r"), Resource::AllKeyspaces);
    assert_eq!(res("GRANT ALL ON ALL ROLES TO r"), Resource::AllRoles);
    assert_eq!(res("GRANT ALL ON ALL FUNCTIONS TO r"), Resource::AllFunctions);
    assert_eq!(res("GRANT ALL ON ALL FUNCTIONS IN KEYSPACE ks TO r"), Resource::AllFunctionsInKeyspace(ident("ks")));
    assert_eq!(res("GRANT ALL ON ALL MBEANS TO r"), Resource::AllMbeans);
    assert!(matches!(res("GRANT EXECUTE ON FUNCTION ks.f(int) TO r"), Resource::Function { arg_types, .. } if arg_types.len() == 1));
    assert!(matches!(res("GRANT ALTER ON ROLE x TO r"), Resource::Role(_)));
    assert!(matches!(res("GRANT SELECT ON t TO r"), Resource::Table { keyword: false, .. }));
    assert!(matches!(res("GRANT SELECT ON MBEANS 'a:type=*' TO r"), Resource::Mbean { plural: true, .. }));
    match c("LIST ALL PERMISSIONS ON KEYSPACE ks OF r NORECURSIVE") {
        StatementKind::ListPermissions(l) => {
            assert_eq!(l.permissions, Permissions::All { keyword: true });
            assert!(l.resource.is_some() && l.of.is_some() && !l.recursive);
        }
        other => panic!("{other:?}"),
    }
    match c("CREATE ROLE IF NOT EXISTS r WITH PASSWORD = 'p' AND LOGIN = true AND OPTIONS = {'a': 1} \
             AND ACCESS TO DATACENTERS {'dc1'} AND ACCESS FROM ALL CIDRS")
    {
        StatementKind::CreateRole(r) => {
            assert!(r.if_exists && r.with);
            assert_eq!(r.options.len(), 5);
            assert_eq!(r.options[1], RoleOption::Login(true));
            assert_eq!(r.options[3], RoleOption::AccessToDatacenters(vec![Cow::Borrowed("dc1")]));
        }
        other => panic!("{other:?}"),
    }
    match c("CREATE USER IF NOT EXISTS u WITH PASSWORD 'p' NOSUPERUSER") {
        StatementKind::CreateUser(u) => {
            assert_eq!(u.password, Some(UserPassword::Plain(Cow::Borrowed("p"))));
            assert_eq!(u.superuser, Some(false));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(c("GRANT r1 TO r2"), StatementKind::GrantRole(_)));
    assert!(matches!(c("REVOKE r1 FROM r2"), StatementKind::RevokeRole(_)));
    assert!(matches!(c("LIST ROLES OF r NORECURSIVE"), StatementKind::ListRoles(l) if !l.recursive));
    assert_eq!(c("LIST USERS"), StatementKind::ListUsers);
    assert!(matches!(c("ADD IDENTITY IF NOT EXISTS 'id' TO ROLE 'r'"), StatementKind::AddIdentity(a) if a.if_not_exists));
    assert!(matches!(c("DROP IDENTITY 'id'"), StatementKind::DropIdentity(_)));
    match s("ATTACH SERVICE LEVEL sl TO r") {
        StatementKind::AttachServiceLevel(a) => assert_eq!((a.role.text.as_ref(), a.grantee.text.as_ref()), ("sl", "r")),
        other => panic!("{other:?}"),
    }
    assert!(matches!(s("LIST EFFECTIVE SERVICE LEVEL OF r"), StatementKind::ListServiceLevels(l) if matches!(*l, ListServiceLevels::EffectiveOf(_))));
    assert!(matches!(s("LIST ALL ATTACHED SERVICE LEVELS"), StatementKind::ListServiceLevels(l) if *l == ListServiceLevels::AllAttached));
    assert!(matches!(s("CREATE SERVICE_LEVEL IF NOT EXISTS sl WITH timeout = 10ms"), StatementKind::CreateServiceLevel(c) if c.if_exists && c.properties.len() == 1));

    let desc = |src| match c(src) {
        StatementKind::Describe(d) => *d,
        other => panic!("{other:?}"),
    };
    assert_eq!(desc("DESC CLUSTER").target, DescribeTarget::Cluster);
    assert!(desc("DESC CLUSTER").short);
    assert_eq!(desc("DESCRIBE FULL SCHEMA").target, DescribeTarget::Schema { full: true });
    assert_eq!(desc("DESCRIBE ONLY KEYSPACE ks").target, DescribeTarget::Keyspace { only: true, name: Some(ident("ks")) });
    assert_eq!(desc("DESCRIBE KEYSPACE").target, DescribeTarget::Keyspace { only: false, name: None });
    assert_eq!(desc("DESCRIBE TABLE ks.t").target, DescribeTarget::Table(qn(Some("ks"), "t")));
    assert_eq!(desc("DESCRIBE ks.t").target, DescribeTarget::Generic(qn(Some("ks"), "t")));
    // A bare keyword is the generic form: what the name refers to.
    assert_eq!(desc("DESCRIBE FUNCTION").target, DescribeTarget::Generic(qn(None, "FUNCTION")));
    assert!(desc("DESCRIBE KEYSPACE ks WITH INTERNALS").with_internals);
    match s("DESCRIBE SCHEMA WITH INTERNALS AND PASSWORDS") {
        StatementKind::Describe(d) => assert!(d.with_internals && d.with_passwords),
        other => panic!("{other:?}"),
    }
}

#[test]
fn names_keep_their_spelling() {
    let sel = select(c("SELECT \"Mixed\"\"Q\", MixedCase FROM \"Ks\".tab"));
    assert_eq!(sel.selectors[0].expr, Selectable::Column(Ident::quoted("Mixed\"Q")));
    let Selectable::Column(id) = &sel.selectors[1].expr else { panic!() };
    assert_eq!((id.text.as_ref(), id.name().as_ref()), ("MixedCase", "mixedcase"));
    assert_eq!(sel.table.keyspace, Some(Ident::quoted("Ks")));
    // Keywords used as names keep their spelling; function names that are keywords keep case.
    let sel = select(c("SELECT Filtering(a), TEXT(b) FROM t"));
    let Selectable::Function(f, _) = &sel.selectors[0].expr else { panic!() };
    assert_eq!(f.resolved(), "Filtering");
    let Selectable::Function(f, _) = &sel.selectors[1].expr else { panic!() };
    assert_eq!(f.resolved(), "text");
}

#[test]
fn bind_markers_are_numbered_in_source_order() {
    let st = one("UPDATE t USING TTL ? SET a = :x, b = ? WHERE k IN ? AND (c, d) IN (?, :y) IF e = :x", Dialect::Cassandra);
    let names: Vec<Option<&str>> = st.bind_markers.iter().map(|n| n.as_ref().map(|i| i.text.as_ref())).collect();
    assert_eq!(names, vec![None, Some("x"), None, None, None, Some("y"), Some("x")]);
    // Markers inside syntactic predicates are not counted twice.
    let st = one("SELECT JSON DISTINCT f(?, :a) FROM t WHERE k = ?", Dialect::Cassandra);
    assert_eq!(st.bind_markers.len(), 3);
}

#[test]
fn scripts_split_on_semicolons() {
    let src = "SELECT * FROM t; ;\n-- comment\nBEGIN BATCH INSERT INTO t (k) VALUES (1); UPDATE t SET v = 1 WHERE k = 1; APPLY BATCH;\
               USE ks";
    let st = parse(src, Dialect::Cassandra).unwrap();
    assert_eq!(st.len(), 3);
    assert!(matches!(st[1].kind, StatementKind::Batch(ref b) if b.statements.len() == 2));
    assert_eq!(&src[st[2].span.start as usize..st[2].span.end as usize], "USE ks");
    assert_eq!(&src[st[0].span.start as usize..st[0].span.end as usize], "SELECT * FROM t");
    // Errors are positioned within their statement, as a client that splits sends them.
    let e = parse("SELECT * FROM t;\nSELECT * FRM t", Dialect::Cassandra).unwrap_err();
    assert!(e.message.starts_with("line 1:9 mismatched input 'FRM'"), "{e}");
    assert_eq!(parse("  ", Dialect::Scylla).unwrap(), vec![]);
    assert_eq!(split_statements("a; b ;; c", Dialect::Scylla), vec![(0, 1), (3, 5), (8, 9)]);
}

#[test]
fn statements_end_at_semicolons_and_one_statement_per_query() {
    assert!(parse_statement("SELECT * FROM t;;;", Dialect::Cassandra).is_ok());
    assert_eq!(
        err("SELECT * FROM t; SELECT * FROM t", Dialect::Cassandra),
        "line 1:17 mismatched input 'SELECT' expecting EOF (SELECT * FROM t; [SELECT]...)"
    );
    assert_eq!(err("SELECT * FROM t; SELECT * FROM t", Dialect::Scylla), "line 1:17 : Syntax error");
}

#[test]
fn errors_carry_code_and_position() {
    let e = parse_statement("SELECT * FRM t", Dialect::Cassandra).unwrap_err();
    assert_eq!((e.code, e.position), (ErrorCode::Syntax, Some((1, 9))));
    let e = parse_statement("SELECT * FROM", Dialect::Cassandra).unwrap_err();
    assert_eq!(e.position, Some((0, -1)));
    let e = parse_statement("SELECT * FROM", Dialect::Scylla).unwrap_err();
    assert_eq!(e.position, Some((1, 0)));
    let e = parse_statement("SELECT * FROM ?.t", Dialect::Scylla).unwrap_err();
    assert_eq!((e.position, e.message.as_str()), (None, "Bind variables cannot be used for keyspace names"));
    let e = parse_statement("CREATE TABLE t (a int PRIMARY KEY, a int)", Dialect::Cassandra).unwrap_err();
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Invalid, "Duplicate column 'a' declaration for table 't'"));
    assert_eq!(e.to_string(), e.message);
}

#[test]
fn into_static_detaches_from_the_text() {
    let owned: Statement<'static> = {
        let text = String::from("SELECT a, 'x' FROM \"K\".t WHERE b = ? AND c IN (1, 2)");
        one(&text, Dialect::Scylla).into_static()
    };
    assert!(matches!(owned.kind, StatementKind::Select(ref s) if s.selectors.len() == 2));
    assert_eq!(owned.bind_markers.len(), 1);
}

#[test]
fn printer_quotes_and_escapes() {
    let st = one("SELECT \"a\"\"b\", 'it''s', $$x$$ FROM t WHERE k = 'a$$b'", Dialect::Cassandra);
    assert_eq!(st.to_string(), "SELECT \"a\"\"b\", 'it''s', $$x$$ FROM t WHERE k = 'a$$b'");
    assert_eq!(display::maybe_quote("abc"), "abc");
    assert_eq!(display::maybe_quote("Abc"), "\"Abc\"");
    assert_eq!(display::maybe_quote("select"), "\"select\"");
    assert_eq!(display::maybe_quote("a\"b"), "\"a\"\"b\"");
    assert_eq!(display::quote_string("it's"), "'it''s'");
    let script = display::script(&parse("USE ks; TRUNCATE t", Dialect::Scylla).unwrap());
    assert_eq!(script, "USE ks;\nTRUNCATE TABLE t;\n");
}

#[test]
fn keyword_lists_per_dialect() {
    let c: Vec<_> = keywords(Dialect::Cassandra).collect();
    let s: Vec<_> = keywords(Dialect::Scylla).collect();
    assert!(c.contains(&"MASKED") && !s.contains(&"MASKED"));
    assert!(s.contains(&"BYPASS") && !c.contains(&"BYPASS"));
    assert!(c.contains(&"SELECT") && s.contains(&"SELECT"));
}

#[test]
fn reserved_and_unreserved_keywords() {
    // Unreserved keywords are identifiers; reserved ones are not.
    for w in ["key", "ttl", "count", "writetime", "json", "distinct", "type", "values", "filtering", "role", "text"] {
        let src = format!("SELECT {w} FROM t WHERE {w} = 1");
        one(&src, Dialect::Cassandra);
        one(&src, Dialect::Scylla);
    }
    for w in ["select", "from", "where", "table", "token", "limit", "primary", "is", "in"] {
        let src = format!("SELECT {w} FROM t");
        assert!(parse_statement(&src, Dialect::Cassandra).is_err(), "{w}");
        assert!(parse_statement(&src, Dialect::Scylla).is_err(), "{w}");
    }
    // Reserved in one dialect only.
    for w in ["cast", "default", "unset", "ann", "replace"] {
        let src = format!("SELECT a AS {w} FROM t");
        one(&src, Dialect::Cassandra);
        assert!(parse_statement(&src, Dialect::Scylla).is_err(), "{w}");
    }
    for w in ["desc", "describe", "execute"] {
        let src = format!("SELECT a AS {w} FROM t");
        one(&src, Dialect::Scylla);
        assert!(parse_statement(&src, Dialect::Cassandra).is_err(), "{w}");
    }
    // ScyllaDB-only keywords are plain identifiers in Cassandra, and unreserved in ScyllaDB.
    for w in ["bypass", "timeout", "service", "level", "prune", "attached"] {
        let src = format!("SELECT {w} FROM t");
        one(&src, Dialect::Cassandra);
        one(&src, Dialect::Scylla);
    }
    // Type names cannot be user type names.
    assert_eq!(err("CREATE TYPE ks.byte (a int)", Dialect::Cassandra), "Invalid (reserved) user type name byte");
    assert!(err("CREATE TYPE ks.int (a int)", Dialect::Cassandra).contains("no viable alternative at input 'int'"));
}

#[test]
fn lexical_rules() {
    let sel = select(c("select A /* block\n comment */ from T -- line\n where X = 1 // more\n"));
    assert_eq!(sel.selectors[0].expr, Selectable::Column(ident("A")));
    assert!(parse_statement("SELECT * FROM t -- no newline", Dialect::Cassandra).is_err());
    assert_eq!(
        err("SELECT * FROM t WHERE a = 'x", Dialect::Cassandra),
        "line 1:28 mismatched character '<EOF>' expecting '''"
    );
    assert_eq!(err("SELECT * FROM t WHERE a = 'x", Dialect::Scylla), "line 1:28 : Syntax error");
    assert_eq!(err("SELECT * FROM t WHERE a = @", Dialect::Cassandra), "line 1:26 no viable alternative at character '@'");
    assert_eq!(err("SELECT * FROM t WHERE a = @", Dialect::Scylla), "line 1:26 no viable alternative at input ''");
    // Durations, ranges and signs.
    let sel = select(c("SELECT * FROM t WHERE a = 1h30m AND b = -1d AND c = PT1H AND d = P1Y2M"));
    assert_eq!(sel.where_clause.len(), 4);
    let sel = select(c("SELECT l[1..2] FROM t"));
    assert!(matches!(&sel.selectors[0].expr, Selectable::Slice(_)));
}

#[test]
fn no_quadratic_behaviour_on_long_input() {
    let n = 50_000;
    let list: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    let src = format!("SELECT * FROM t WHERE k IN ({})", list.join(", "));
    let start = std::time::Instant::now();
    one(&src, Dialect::Cassandra);
    one(&src, Dialect::Scylla);
    assert!(start.elapsed().as_secs() < 10, "{:?}", start.elapsed());
    // Deep nesting is bounded rather than overflowing the stack.
    let deep = format!("SELECT * FROM t WHERE k = {}1{}", "(".repeat(5000), ")".repeat(5000));
    assert!(parse_statement(&deep, Dialect::Cassandra).is_err());
    let deep = format!("SELECT {}a{} FROM t", "[".repeat(5000), "]".repeat(5000));
    assert!(parse_statement(&deep, Dialect::Scylla).is_err());
}

/// ScyllaDB takes `- Infinity` / `- NaN` as a plain assignment value (probed); any other `-`
/// prefix stays a syntax error.
#[test]
fn scylla_assignment_of_negative_infinity() {
    for src in ["UPDATE t SET d = -infinity WHERE p = 1", "UPDATE t SET d = - NaN WHERE p = 1"] {
        let StatementKind::Update(u) = s(src) else { panic!("{src}") };
        assert!(matches!(&u.assignments[0].op, Operation::Set(Term::Literal(_))), "{src}");
    }
    assert!(err("UPDATE t SET d = -x WHERE p = 1", Dialect::Scylla).contains("Unexpected 'x'"));
}

/// ScyllaDB's property map values may be lists of constants (rack lists) or `null`; Cassandra
/// takes neither.
#[test]
fn scylla_property_map_lists_and_null() {
    let src = "CREATE KEYSPACE k WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': ['rack1', 'r2'], 'dc2': []}";
    let StatementKind::CreateKeyspace(k) = s(src) else { panic!() };
    let PropertyValue::Map(m) = &k.properties[0].value else { panic!() };
    assert_eq!((m[1].1.kind, m[1].1.list_items()), (LiteralKind::List, vec!["rack1", "r2"]));
    assert_eq!((m[2].1.kind, m[2].1.list_items()), (LiteralKind::List, Vec::<&str>::new()));
    round_trip(src, Dialect::Scylla);
    assert!(err(src, Dialect::Cassandra).starts_with("Invalid property value: ['rack1', 'r2']"));
    let src = "ALTER TABLE t WITH compression = {'sstable_compression': 'LZ4Compressor', 'chunk_length_in_kb': null}";
    let StatementKind::AlterTable(a) = s(src) else { panic!() };
    let AlterTableAction::With(props) = &a.action else { panic!() };
    let PropertyValue::Map(m) = &props[0].value else { panic!() };
    assert_eq!((m[1].1.kind, m[1].1.text.as_ref()), (LiteralKind::String, "null"));
    assert!(err(src, Dialect::Cassandra).starts_with("Invalid property value: NULL"));
}
