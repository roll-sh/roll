//! Cassandra 5's math, collection, masking, `floor` and JSON-alias functions, and ScyllaDB's
//! `set_intersection`: every overload family, null handling and error path, with the values and
//! messages observed on `cassandra:5.0.9` / `scylladb/scylla:2026.3.3` (cqlsh on the references,
//! keyspace `udf_m`). ScyllaDB has none of Cassandra's: "Unknown function X called".

use super::*;

const NUMS: &str = "CREATE TABLE ks.t (pk int PRIMARY KEY, ti tinyint, si smallint, i int, bi bigint, f float, d double, vi varint, de decimal, l list<int>, s set<text>, m map<text, int>, txt text, a ascii, b blob, fl frozen<list<int>>)";

async fn numbers(d: Dialect) -> H {
    let mut h = H::new(d).await;
    h.ok(KS).await;
    h.ok(NUMS).await;
    h.ok("INSERT INTO ks.t (pk, ti, si, i, bi, f, d, vi, de, l, s, m, txt, a, b, fl) VALUES (1, -128, -32768, -2147483648, -9223372036854775808, -2.5, -2.5, -123456789012345678901234567890, -2.5, [3,1,2], {'b','a'}, {'x':1,'y':2}, 'hello', 'abc', 0x0102, [5,6])").await;
    h.ok("INSERT INTO ks.t (pk, ti, si, i, bi, f, d, vi, de) VALUES (2, 100, 30000, 50, 50, 0.49999997, 2.5, 0, 0)").await;
    h.ok("INSERT INTO ks.t (pk, ti, si, i, bi, f, d, vi, de) VALUES (3, 5, 5, 5, 5, 1.5, 1e300, 1000, 12.345678901234567890123456789012345678901234567890)").await;
    h.ok("INSERT INTO ks.t (pk) VALUES (9)").await;
    h
}

#[tokio::test]
async fn scylla_has_none_of_them() {
    let mut h = numbers(Dialect::Scylla).await;
    for f in [
        "abs(i)",
        "exp(i)",
        "log(i)",
        "log10(i)",
        "round(i)",
        "map_keys(m)",
        "map_values(m)",
        "collection_count(l)",
        "collection_min(l)",
        "collection_max(l)",
        "collection_sum(l)",
        "collection_avg(l)",
        "mask_null(i)",
        "mask_default(i)",
        "mask_replace(i, 1)",
        "mask_inner(txt, 1, 1)",
        "mask_outer(txt, 1, 1)",
        "mask_hash(txt)",
        "floor(i, 1h)",
        "to_json(i)",
    ] {
        let name = f.split('(').next().unwrap();
        assert_eq!(
            h.msg(&format!("SELECT {f} FROM ks.t")).await,
            format!("Unknown function {name} called"),
            "{f}"
        );
    }
}

#[tokio::test]
async fn math_functions() {
    let mut h = numbers(Dialect::Cassandra).await;
    let q = "FROM ks.t WHERE pk IN (1, 2, 3)";
    assert_eq!(
        h.rows(&format!(
            "SELECT abs(ti), abs(si), abs(i), abs(bi), abs(f), abs(d), abs(vi), abs(de) {q}"
        ))
        .await,
        table(&[
            &[
                "-128",
                "-32768",
                "-2147483648",
                "-9223372036854775808",
                "2.5",
                "2.5",
                "123456789012345678901234567890",
                "2.5"
            ],
            &["100", "30000", "50", "50", "0.5", "2.5", "0", "0"],
            &[
                "5",
                "5",
                "5",
                "5",
                "1.5",
                "1e+300",
                "1000",
                "12.345678901234567890123456789012345678901234567890"
            ],
        ])
    );
    // Doubles as Java's `Double.toString` prints them (cqlsh rounds them for display).
    assert_eq!(
        h.rows(&format!(
            "SELECT log(ti), log(si), log(i), log(bi), log(f), CAST(log(d) AS text) {q}"
        ))
        .await,
        table(&[
            &["0", "0", "0", "0", "NaN", "NaN"],
            &["4", "10", "3", "3", "-0.693147", "0.9162907318741551"],
            &["1", "1", "1", "1", "0.405465", "690.7755278982137"],
        ])
    );
    assert_eq!(
        h.rows(&format!(
            "SELECT log10(ti), log10(si), log10(i), log10(bi), log10(f), CAST(log10(d) AS text) {q}"
        ))
        .await,
        table(&[
            &["0", "0", "0", "0", "NaN", "NaN"],
            &["2", "4", "1", "1", "-0.30103", "0.3979400086720376"],
            &["0", "0", "0", "0", "0.176091", "300.0"],
        ])
    );
    assert_eq!(
        h.rows(&format!("SELECT round(ti), round(si), round(i), round(bi), round(f), round(d), round(vi), round(de) {q}")).await,
        table(&[
            &["-128", "-32768", "-2147483648", "-9223372036854775808", "-2", "-2", "-123456789012345678901234567890", "-3"],
            &["100", "30000", "50", "50", "0", "3", "0", "0"],
            &["5", "5", "5", "5", "2", "9.22337203685e+18", "1000", "12"],
        ])
    );
    assert_eq!(
        h.rows("SELECT log(vi), log10(vi), log(de), log10(de) FROM ks.t WHERE pk = 3")
            .await,
        table(&[&[
            "6",
            "3",
            "2.5133061153096982450729931651729880898393297539523",
            "1.0915149772126998957108302782343211743723014330733"
        ]])
    );
    assert_eq!(
        h.rows("SELECT CAST(exp(de) AS text), exp(vi) FROM ks.t WHERE pk = 3")
            .await[0][0],
        "229964.19485298854521264777192887338471140368572850"
    );
    assert_eq!(
        h.rows("SELECT exp(vi), CAST(exp(de) AS text) FROM ks.t WHERE pk = 2")
            .await,
        table(&[&["1", "1"]])
    );
    assert_eq!(
        h.rows("SELECT CAST(exp(de) AS text) FROM ks.t WHERE pk = 1")
            .await,
        table(&[&["0.082084998623898795169528674467160"]])
    );
    // Errors are uncaught `ArithmeticException`s.
    for (cql, msg) in [
        (
            "SELECT exp(vi) FROM ks.t WHERE pk = 1",
            "java.lang.ArithmeticException: Overflow",
        ),
        (
            "SELECT log(vi) FROM ks.t WHERE pk = 2",
            "java.lang.ArithmeticException: Natural log of number zero or less",
        ),
        (
            "SELECT log(de) FROM ks.t WHERE pk = 2",
            "java.lang.ArithmeticException: Natural log of number zero or less",
        ),
        (
            "SELECT log10(de) FROM ks.t WHERE pk = 1",
            "java.lang.ArithmeticException: Log10 of number zero or less",
        ),
    ] {
        let e = h.err(cql).await;
        assert_eq!(
            (e.code, e.message.as_str()),
            (ErrorCode::Server, msg),
            "{cql}"
        );
    }
    // Nulls give null; literals and markers resolve like other overloaded functions.
    assert_eq!(
        h.rows("SELECT abs(i), exp(de), round(f) FROM ks.t WHERE pk = 9")
            .await,
        table(&[&["null", "null", "null"]])
    );
    assert_eq!(
        h.rows("SELECT abs(1), round(1.5), abs(-3), exp(0) FROM ks.t WHERE pk = 9")
            .await,
        table(&[&["1", "2", "3", "1"]])
    );
    let e = h.msg("SELECT abs(?) FROM ks.t").await;
    assert!(
        e.starts_with(
            "Ambiguous call to function abs (can be matched by following signatures: system.abs : ("
        ),
        "{e}"
    );
    let e = h.msg("SELECT abs(txt) FROM ks.t").await;
    assert!(e.starts_with("Invalid call to function abs, none of its type signatures match (known type signatures: system.abs : ("), "{e}");
    assert_eq!(
        h.specs("SELECT abs(ti), exp(si), log(vi), round(de) FROM ks.t WHERE pk = 3")
            .await,
        vec![
            ("system.abs(ti)".into(), "tinyint".into()),
            ("system.exp(si)".into(), "smallint".into()),
            ("system.log(vi)".into(), "varint".into()),
            ("system.round(de)".into(), "decimal".into()),
        ]
    );
    // Counters.
    h.ok("CREATE TABLE ks.c (pk int PRIMARY KEY, c counter)")
        .await;
    h.ok("UPDATE ks.c SET c = c - 7 WHERE pk = 1").await;
    assert_eq!(
        h.specs("SELECT abs(c) FROM ks.c").await,
        vec![("system.abs(c)".into(), "counter".into())]
    );
    assert_eq!(
        h.rows("SELECT abs(c), round(c) FROM ks.c").await,
        table(&[&["7", "-7"]])
    );
}

#[tokio::test]
async fn decimal_precision_follows_big_math() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.ok(KS).await;
    h.ok("CREATE TABLE ks.d (pk int PRIMARY KEY, de decimal, vi varint)")
        .await;
    for (pk, de, vi) in [
        (1, "100", "100"),
        (2, "1", "1"),
        (3, "10", "10"),
        (4, "0.001", "-5"),
        (5, "1E+10", "123456789012345678901234567890123456789"),
        (6, "1000.5", "2"),
        (7, "-1000.5", "50"),
        (8, "2.5", "-100"),
    ] {
        h.ok(&format!(
            "INSERT INTO ks.d (pk, de, vi) VALUES ({pk}, {de}, {vi})"
        ))
        .await;
    }
    let s = "CAST(exp(de) AS text), CAST(log(de) AS text), CAST(log10(de) AS text), CAST(round(de) AS text), CAST(abs(de) AS text)";
    assert_eq!(
        h.rows(&format!(
            "SELECT pk, {s} FROM ks.d WHERE pk IN (1, 2, 3, 6)"
        ))
        .await,
        table(&[
            &[
                "1",
                "2.6881171418161354484126255515800E+43",
                "4.6051701859880913680359829093687",
                "2",
                "100",
                "100"
            ],
            &[
                "2",
                "2.7182818284590452353602874713527",
                "0",
                "0E+33",
                "1",
                "1"
            ],
            &[
                "3",
                "22026.465794806716516957900645284",
                "2.3025850929940456840179914546844",
                "1",
                "10",
                "10"
            ],
            &[
                "6",
                "3.2480981504718027596309675316942E+434",
                "6.9082551540237880999680379796365",
                "3.0002170929722302082819128837511",
                "1001",
                "1000.5"
            ],
        ])
    );
    assert_eq!(
        h.rows("SELECT pk, CAST(exp(de) AS text), CAST(round(de) AS text) FROM ks.d WHERE pk IN (4, 7, 8)").await,
        table(&[
            &["4", "1.0010005001667083416680557539931", "0"],
            &["7", "3.0787246988048834641772868393638E-435", "-1001"],
            &["8", "12.182493960703473438070175951168", "3"],
        ])
    );
    assert_eq!(
        h.rows(
            "SELECT pk, CAST(log(de) AS text), CAST(log10(de) AS text) FROM ks.d WHERE pk IN (4, 5)"
        )
        .await,
        table(&[
            &[
                "4",
                "-6.9077552789821370520539743640531",
                "-3.0000000000000000000000000000000"
            ],
            &["5", "23.025850929940456840179914546844", "1E+1"],
        ])
    );
    assert_eq!(
        h.rows("SELECT pk, exp(vi), log(vi), log10(vi) FROM ks.d WHERE pk IN (1, 2, 3, 6, 7)")
            .await,
        table(&[
            &[
                "1",
                "26881171418161354484126255515800000000000000",
                "4",
                "2"
            ],
            &["2", "2", "0", "0"],
            &["3", "22026", "2", "1"],
            &["6", "7", "0", "0"],
            &["7", "5184705528587072464087", "3", "1"],
        ])
    );
    assert_eq!(
        h.rows("SELECT pk, abs(vi), round(vi), log(vi), log10(vi) FROM ks.d WHERE pk IN (5)")
            .await[0][2..],
        [
            "123456789012345678901234567890123456789".to_string(),
            "87".into(),
            "38".into()
        ]
    );
    assert_eq!(
        h.rows("SELECT pk, exp(vi), abs(vi), round(vi) FROM ks.d WHERE pk IN (4, 8)")
            .await,
        table(&[&["4", "0", "5", "-5"], &["8", "0", "100", "-100"]])
    );
    assert_eq!(
        h.err("SELECT exp(de) FROM ks.d WHERE pk = 5").await.message,
        "java.lang.ArithmeticException: Overflow"
    );
}

#[tokio::test]
async fn collection_functions() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.ok(KS).await;
    h.ok("CREATE TYPE ks.ad (x int)").await;
    h.ok("CREATE TABLE ks.o (pk int PRIMARY KEY, tu tuple<int, text>, u frozen<ad>, v vector<float, 2>, ml map<int, text>, st set<int>, li list<text>, mf frozen<map<int, int>>, fl frozen<list<int>>)").await;
    h.ok("INSERT INTO ks.o (pk, tu, u, v, ml, st, li, mf, fl) VALUES (1, (1, 'a'), {x: 1}, [1.0, 2.0], {1: 'a', 2: 'b'}, {3, 1, 2}, ['b', 'a', 'c'], {}, [])").await;
    h.ok("INSERT INTO ks.o (pk) VALUES (2)").await;
    assert_eq!(
        h.rows("SELECT map_keys(ml), map_values(ml), collection_count(ml), collection_count(st), collection_count(li), collection_min(st), collection_max(li), collection_sum(st), collection_avg(st) FROM ks.o WHERE pk = 1").await,
        table(&[&["{1, 2}", "['a', 'b']", "2", "3", "3", "1", "c", "6", "2"]])
    );
    assert_eq!(
        h.specs("SELECT map_keys(ml), map_values(ml), collection_count(ml), collection_min(li), collection_avg(st) FROM ks.o").await,
        vec![
            ("system.map_keys(ml)".into(), "frozen<set<int>>".into()),
            ("system.map_values(ml)".into(), "frozen<list<text>>".into()),
            ("system.collection_count(ml)".into(), "int".into()),
            ("system.collection_min(li)".into(), "text".into()),
            ("system.collection_avg(st)".into(), "int".into()),
        ]
    );
    // Empty frozen collections: count 0, min null, avg 0; a missing collection is null.
    assert_eq!(
        h.rows("SELECT collection_count(fl), collection_min(fl), collection_sum(fl), collection_avg(fl), collection_count(mf), map_keys(mf) FROM ks.o WHERE pk = 1").await,
        table(&[&["0", "null", "0", "0", "0", "{}"]])
    );
    assert_eq!(
        h.rows(
            "SELECT map_keys(ml), collection_count(li), collection_max(st) FROM ks.o WHERE pk = 2"
        )
        .await,
        table(&[&["null", "null", "null"]])
    );
    assert_eq!(
        h.rows("SELECT collection_count([1, 2]), collection_sum([1, 2, 3]) FROM ks.o WHERE pk = 1")
            .await,
        table(&[&["2", "6"]])
    );
    for (cql, msg) in [
        (
            "SELECT map_keys(st) FROM ks.o",
            "Function system.map_keys requires a map argument, but found argument st of type set<int>",
        ),
        (
            "SELECT map_values(li) FROM ks.o",
            "Function system.map_values requires a map argument, but found argument li of type list<text>",
        ),
        (
            "SELECT collection_count(tu) FROM ks.o",
            "Function system.collection_count requires a collection argument, but found argument tu of type frozen<tuple<int, text>>",
        ),
        (
            "SELECT collection_min(ml) FROM ks.o",
            "Function system.collection_min requires a set or list argument, but found argument ml of type map<int, text>",
        ),
        (
            "SELECT collection_max(mf) FROM ks.o",
            "Function system.collection_max requires a set or list argument, but found argument mf of type frozen<map<int, int>>",
        ),
        (
            "SELECT collection_sum(li) FROM ks.o",
            "Function system.collection_sum requires a numeric set/list argument, but found argument li of type list<text>",
        ),
        (
            "SELECT collection_sum(ml) FROM ks.o",
            "Function system.collection_sum requires a numeric set/list argument, but found argument ml of type map<int, text>",
        ),
        (
            "SELECT collection_avg(v) FROM ks.o",
            "Function system.collection_avg requires a numeric set/list argument, but found argument v of type vector<float, 2>",
        ),
        (
            "SELECT map_keys(ml, ml) FROM ks.o",
            "Invalid number of arguments for function system.map_keys(map)",
        ),
        (
            "SELECT collection_count() FROM ks.o",
            "Invalid number of arguments for function system.collection_count(collection)",
        ),
        (
            "SELECT collection_min(st, st) FROM ks.o",
            "Invalid number of arguments for function system.collection_min(numeric_set_or_list)",
        ),
        (
            "SELECT map_keys(?) FROM ks.o",
            "Cannot infer type of argument ? in call to function system.map_keys(map): use type casts to disambiguate",
        ),
    ] {
        assert_eq!(h.msg(cql).await, msg, "{cql}");
    }
    // `anyMap` lets a UDT through; the factory's cast fails.
    let e = h.err("SELECT map_keys(u) FROM ks.o").await;
    assert_eq!(e.code, ErrorCode::Server);
    assert!(e.message.starts_with("java.lang.ClassCastException: class org.apache.cassandra.db.marshal.UserType cannot be cast to class org.apache.cassandra.db.marshal.MapType"), "{}", e.message);
}

#[tokio::test]
async fn masking_functions() {
    let mut h = numbers(Dialect::Cassandra).await;
    assert_eq!(
        h.rows("SELECT mask_default(txt), mask_default(i), mask_default(l), mask_null(txt), mask_replace(txt, 'x'), mask_inner(txt, 1, 1), mask_hash(txt) FROM ks.t WHERE pk = 9").await,
        table(&[&["****", "0", "[]", "null", "x", "null", "null"]])
    );
    assert_eq!(
        h.rows("SELECT mask_default(ti), mask_default(si), mask_default(bi), mask_default(f), mask_default(d), mask_default(vi), mask_default(de), mask_default(b), mask_default(a), mask_default(m), mask_default(s), mask_default(fl) FROM ks.t WHERE pk = 1").await,
        table(&[&["0", "0", "0", "0", "0", "0", "0", "0x", "****", "{}", "{}", "[]"]])
    );
    assert_eq!(
        h.rows("SELECT mask_inner(txt, 1, 1, '#'), mask_outer(txt, 2, 0, '#'), mask_inner(txt, 10, 10), mask_outer(txt, 10, 10), mask_inner(txt, -1, 2), mask_outer(txt, null, 1), mask_inner(a, 1, 1) FROM ks.t WHERE pk = 1").await,
        table(&[&["h###o", "##llo", "hello", "*****", "***lo", "hell*", "a*c"]])
    );
    assert_eq!(
        h.specs(
            "SELECT mask_inner(a, 1, 1), mask_hash(i), mask_replace(l, [9]), mask_null(m) FROM ks.t"
        )
        .await,
        vec![
            ("system.mask_inner(a, 1, 1)".into(), "ascii".into()),
            ("system.mask_hash(i)".into(), "blob".into()),
            ("system.mask_replace(l, [9])".into(), "list<int>".into()),
            ("system.mask_null(m)".into(), "map<text, int>".into()),
        ]
    );
    assert_eq!(
        h.rows("SELECT mask_hash(txt), mask_hash(txt, 'MD5'), mask_hash(txt, 'SHA-1'), mask_hash(txt, 'SHA3-256'), mask_hash(txt, null) FROM ks.t WHERE pk = 1").await,
        table(&[&[
            "0x2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "0x5d41402abc4b2a76b9719d911017c592",
            "0xaaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d",
            "0x3338be694f50c5f338814986cdf0686453a888b84f424d792af4b9202398f392",
            "0x2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        ]])
    );
    assert_eq!(
        h.rows("SELECT mask_hash(i, 'sha-512') FROM ks.t WHERE pk = 1")
            .await[0][0],
        "0xdf45732659aad83aae5edfc59bab32796c708eb61e9be97ab94abbf7616d9b3a36fba7c84aedcfe1857b54edb41e169b1959d7d7d69d8349a38d516947172ce0"
    );
    assert_eq!(
        h.rows("SELECT mask_replace(i, 7), mask_replace(txt, null), mask_replace(l, [9]), mask_null(1), mask_default(1), mask_replace(1, 2) FROM ks.t WHERE pk = 1").await,
        table(&[&["7", "null", "[9]", "null", "0", "2"]])
    );
    for (cql, msg) in [
        (
            "SELECT mask_inner(txt, 1, 1, '##') FROM ks.t WHERE pk = 1",
            "The padding argument for function system.mask_inner should be single-character, but '##' has 2 characters.",
        ),
        (
            "SELECT mask_outer(txt, 1, 1, '') FROM ks.t WHERE pk = 1",
            "The padding argument for function system.mask_outer should be single-character, but '' has 0 characters.",
        ),
        (
            "SELECT mask_inner(i, 1, 1) FROM ks.t",
            "Function system.mask_inner requires an argument of type [text|varchar|ascii], but found argument i of type int",
        ),
        (
            "SELECT mask_inner(txt, 1) FROM ks.t",
            "Invalid number of arguments for function system.mask_inner([text|varchar|ascii], int, int, [text])",
        ),
        (
            "SELECT mask_outer(txt, 1, 2, '#', 3) FROM ks.t",
            "Invalid number of arguments for function system.mask_outer([text|varchar|ascii], int, int, [text])",
        ),
        (
            "SELECT mask_inner(txt, 'a', 1) FROM ks.t",
            "Function system.mask_inner requires an argument of type int, but found argument 'a' of type ascii",
        ),
        (
            "SELECT mask_hash(txt, 'nope') FROM ks.t WHERE pk = 1",
            "Hash algorithm not found: nope",
        ),
        (
            "SELECT mask_hash(txt, 1) FROM ks.t",
            "Function system.mask_hash requires an argument of type text, but found argument 1 of type int",
        ),
        (
            "SELECT mask_hash() FROM ks.t",
            "Invalid number of arguments for function system.mask_hash(any, [text])",
        ),
        (
            "SELECT mask_replace(i, 'x') FROM ks.t",
            "Type error: 'x' cannot be passed as argument 1 of function system.mask_replace of type int",
        ),
        (
            "SELECT mask_replace(i) FROM ks.t",
            "Invalid number of arguments for function system.mask_replace(any, any)",
        ),
        (
            "SELECT mask_null(?) FROM ks.t",
            "Cannot infer type of argument ? in call to function system.mask_null(any): use type casts to disambiguate",
        ),
        (
            "SELECT mask_null(txt, 1) FROM ks.t",
            "Invalid number of arguments for function system.mask_null(any)",
        ),
        (
            "SELECT mask_default(txt, 1) FROM ks.t",
            "Invalid number of arguments for function system.mask_default(any)",
        ),
    ] {
        assert_eq!(h.msg(cql).await, msg, "{cql}");
    }
    // Every type has a masked value.
    h.ok("CREATE TYPE ks.ad (x int)").await;
    h.ok("CREATE TABLE ks.o (pk int PRIMARY KEY, tu tuple<int, text>, u frozen<ad>, v vector<float, 2>, du duration, ip inet, tm time, dt date, tid timeuuid, id uuid, bo boolean, ts timestamp)").await;
    h.ok("INSERT INTO ks.o (pk, tu, u, v, du, ip, tm, dt, tid, id, bo, ts) VALUES (1, (1, 'a'), {x: 1}, [1.0, 2.0], 1h, '1.2.3.4', '01:00:00', '2020-01-01', now(), uuid(), true, 0)").await;
    assert_eq!(
        h.rows("SELECT mask_default(tu), mask_default(u), mask_default(v), mask_default(du), mask_default(ip), mask_default(tm), mask_default(dt), mask_default(tid), mask_default(id), mask_default(bo), mask_default(ts) FROM ks.o").await,
        table(&[&[
            "(0, '****')",
            "{x: 0}",
            "[0, 0]",
            "",
            "0.0.0.0",
            "00:00:00.000000000",
            "1970-01-01",
            "13814000-1dd2-11b2-8080-808080808080",
            "00000000-0000-0000-0000-000000000000",
            "False",
            "1970-01-01 00:00:00.000000+0000",
        ]])
    );
}

#[tokio::test]
async fn floor_functions() {
    let mut h = H::new(Dialect::Cassandra).await;
    h.ok(KS).await;
    h.ok("CREATE TABLE ks.f (pk int PRIMARY KEY, ts timestamp, dt date, tm time, tu timeuuid)")
        .await;
    h.ok("INSERT INTO ks.f (pk, ts, dt, tm, tu) VALUES (1, '2020-05-05 13:47:12.345+0000', '2020-05-05', '13:47:12.345678901', 50554d6e-29bb-11e5-b345-feff819cdc9f)").await;
    assert_eq!(
        h.rows("SELECT floor(ts, 1h), floor(dt, 2d), floor(tm, 1m), floor(tu, 1h), floor(ts, 1h, '2020-01-01') FROM ks.f").await,
        table(&[&[
            "2020-05-05 13:00:00.000000+0000",
            "2020-05-04",
            "13:47:00.000000000",
            "2015-07-14 00:00:00.000000+0000",
            "2020-05-05 13:00:00.000000+0000",
        ]])
    );
    assert_eq!(
        h.rows("SELECT floor(ts, 1mo), floor(ts, 1mo, '2019-12-31'), floor(dt, 1y) FROM ks.f")
            .await,
        table(&[&[
            "2020-05-01 00:00:00.000000+0000",
            "2020-05-31 00:00:00.000000+0000",
            "2020-01-01",
        ]])
    );
    for (cql, msg) in [
        (
            "SELECT floor(ts, 1us) FROM ks.f",
            "The floor cannot be computed for the 1us duration as precision is below 1 millisecond",
        ),
        (
            "SELECT floor(dt, 1h) FROM ks.f",
            "The floor on date values cannot be computed for the 1h duration as precision is below 1 day",
        ),
        (
            "SELECT floor(ts, 1h, '2030-01-01') FROM ks.f",
            "The floor function starting time is greater than the provided time",
        ),
        (
            "SELECT floor(ts, -1h) FROM ks.f",
            "Negative durations are not supported by the floor function",
        ),
        (
            "SELECT floor(tm, 2d) FROM ks.f",
            "For time values, the floor can only be computed for durations smaller that a day",
        ),
    ] {
        assert_eq!(h.msg(cql).await, msg, "{cql}");
    }
}

#[tokio::test]
async fn json_aliases_and_legacy_names() {
    let mut h = numbers(Dialect::Cassandra).await;
    assert_eq!(
        h.rows("SELECT to_json(pk), tojson(txt) FROM ks.t WHERE pk = 1")
            .await,
        table(&[&["1", "\"hello\""]])
    );
    h.ok("INSERT INTO ks.t (pk, i) VALUES (5, from_json('7'))")
        .await;
    assert_eq!(
        h.rows("SELECT i FROM ks.t WHERE pk = 5").await,
        table(&[&["7"]])
    );
    assert_eq!(
        h.msg("SELECT to_json(pk, pk) FROM ks.t").await,
        "Invalid number of arguments for function system.to_json(any)"
    );
    assert_eq!(
        h.msg("SELECT from_json(txt) FROM ks.t").await,
        "from_json() cannot be used in the selection clause of a SELECT statement"
    );
    assert_eq!(
        h.msg("SELECT dateof(now()) FROM ks.t").await,
        "Unknown function 'dateof'"
    );
    let mut s = numbers(Dialect::Scylla).await;
    assert_eq!(
        s.msg("SELECT current_date() FROM ks.t").await,
        "Unknown function current_date called"
    );
    assert_eq!(
        s.rows("SELECT unixtimestampof(maxtimeuuid(0)) FROM ks.t WHERE pk = 1")
            .await,
        table(&[&["0"]])
    );
}

#[tokio::test]
async fn scylla_set_intersection() {
    let mut h = H::new(Dialect::Scylla).await;
    h.ok(KS).await;
    h.ok("CREATE TABLE ks.s (pk int PRIMARY KEY, a set<int>, b set<int>, c frozen<set<int>>, t set<text>)").await;
    h.ok("INSERT INTO ks.s (pk, a, b, c, t) VALUES (1, {1, 2, 3}, {2, 3, 4}, {3}, {'x'})")
        .await;
    h.ok("INSERT INTO ks.s (pk, a) VALUES (2, {1})").await;
    assert_eq!(
        h.rows("SELECT pk, set_intersection(a, b), set_intersection(a, c), set_intersection(c, a) FROM ks.s").await,
        table(&[&["1", "{2, 3}", "{3}", "{3}"], &["2", "null", "null", "null"]])
    );
    assert_eq!(
        h.rows("SELECT set_intersection(a, {2, 9}), set_intersection(a, {7}), system.set_intersection(a, b) FROM ks.s WHERE pk = 1").await,
        table(&[&["{2}", "{}", "{2, 3}"]])
    );
    assert_eq!(
        h.columns("SELECT set_intersection({7}, {7, 8}) FROM ks.s WHERE pk = 1")
            .await,
        vec!["{7}".to_string()]
    );
    assert!(
        h.s.prepare("SELECT set_intersection(?, a) FROM ks.s WHERE pk = 1", None)
            .await
            .is_ok()
    );
    for (cql, msg) in [
        (
            "SELECT set_intersection(a) FROM ks.s",
            "set_intersection() accepts 2 arguments only",
        ),
        (
            "SELECT set_intersection(a, a, a) FROM ks.s",
            "set_intersection() accepts 2 arguments only",
        ),
        (
            "SELECT set_intersection(?, ?) FROM ks.s",
            "set_intersection() can only be called if at least one argument type is known",
        ),
        (
            "SELECT set_intersection(pk, a) FROM ks.s",
            "set_intersection() can only be called if both arguments are of set type",
        ),
        (
            "SELECT set_intersection(a, [1]) FROM ks.s",
            "set_intersection() can only be called if both arguments are of set type",
        ),
        (
            "SELECT set_intersection(a, t) FROM ks.s",
            "set_intersection() can only be called if both arguments are of the same set type: [\"org.apache.cassandra.db.marshal.FrozenType(org.apache.cassandra.db.marshal.SetType(org.apache.cassandra.db.marshal.Int32Type))\", \"org.apache.cassandra.db.marshal.FrozenType(org.apache.cassandra.db.marshal.SetType(org.apache.cassandra.db.marshal.UTF8Type))\"]",
        ),
    ] {
        assert_eq!(h.msg(cql).await, msg, "{cql}");
    }
    let mut c = H::new(Dialect::Cassandra).await;
    c.ok(KS).await;
    c.ok("CREATE TABLE ks.s (pk int PRIMARY KEY, a set<int>)")
        .await;
    assert_eq!(
        c.msg("SELECT set_intersection(a, a) FROM ks.s").await,
        "Unknown function 'set_intersection'"
    );
}
