//! WHERE-clause, selector and write semantics probed on both references (see
//! `probed_harness.rs`): the cases behind the cqlpy categories of
//! `docs/nosql/conformance-triage.md` (`filtering`, `allow_filtering`, `null_handling`, …).

use super::probed_harness::{V, X, both, bound, c, check};

const SYNTAX: X = X::Code(0x2000);
const NONE: X = X::Rows(&[]);
const FILTERING: &str = "Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING";

/// `NOT IN` (ScyllaDB only): on every key kind, with markers, and with nulls (a null in the
/// list matches nothing).
#[tokio::test]
async fn not_in() {
    let setup = "CREATE TABLE t2 (p1 int, p2 int, c int, v int, PRIMARY KEY ((p1, p2), c));\n\
                 INSERT INTO t2 (p1, p2, c, v) VALUES (1, 2, 3, 4);\n\
                 INSERT INTO t2 (p1, p2, c, v) VALUES (3, 4, 5, 6);\n\
                 INSERT INTO t2 (p1, p2, c, v) VALUES (4, 5, 6, 7)";
    let all: X = X::Rows(&[
        &["3", "4", "5", "6"],
        &["4", "5", "6", "7"],
        &["1", "2", "3", "4"],
    ]);
    check(
        setup,
        &[
            c(
                "SELECT * FROM t2 WHERE p1 NOT IN (1, 3, 4) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE p2 NOT IN (2, 4, 5) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE c NOT IN (3, 5, 6) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE v NOT IN (4, 6, 7) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE p1 NOT IN (1) ALLOW FILTERING",
                X::Rows(&[&["3", "4", "5", "6"], &["4", "5", "6", "7"]]),
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE p1 = 1 AND p2 = 2 AND c NOT IN (3)",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE p1 = 1 AND p2 = 2 AND c NOT IN (7) ALLOW FILTERING",
                X::Rows(&[&["1", "2", "3", "4"]]),
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE p1 NOT IN (1, 3) AND v NOT IN (7) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE c NOT IN (3) AND c NOT IN (5, 6) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE v NOT IN () ALLOW FILTERING",
                all,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE v NOT IN (null) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE v NOT IN (1, null) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE p1 NOT IN (1, null) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            c(
                "SELECT * FROM t2 WHERE c NOT IN (null) ALLOW FILTERING",
                NONE,
                SYNTAX,
            ),
            bound(
                "SELECT * FROM t2 WHERE v NOT IN (?, ?) ALLOW FILTERING",
                &[V::I(4), V::I(6)],
                X::Rows(&[&["4", "5", "6", "7"]]),
                SYNTAX,
            ),
            bound(
                "SELECT * FROM t2 WHERE v NOT IN (?, ?) ALLOW FILTERING",
                &[V::I(1), V::Null],
                NONE,
                SYNTAX,
            ),
            bound(
                "SELECT * FROM t2 WHERE v NOT IN ? ALLOW FILTERING",
                &[V::Null],
                NONE,
                SYNTAX,
            ),
        ],
    )
    .await;
}

/// `!=`, `token()` arity and order, subscripts, durations and non-frozen UDTs.
#[tokio::test]
async fn relation_errors() {
    let setup = "CREATE TABLE t (a int PRIMARY KEY, b int);\n\
        CREATE TABLE t2 (pk1 int, pk2 int, c int, s set<int>, l list<int>, v int, PRIMARY KEY ((pk1, pk2), c));\n\
        CREATE TYPE ud (a int, d duration);\n\
        CREATE TYPE ui (a int, b int);\n\
        CREATE TABLE t3 (p int PRIMARY KEY, u ud, x ui, fx frozen<ui>);\n\
        INSERT INTO t3 (p, x) VALUES (1, {a: 1, b: 2});\n\
        CREATE TABLE t4 (p int PRIMARY KEY, d duration, fl frozen<list<duration>>, tu tuple<int, duration>, u ud, fu frozen<ud>)";
    let nonfrozen_x = "Non-frozen UDT column 'x' (ui) cannot be restricted by any relation";
    check(
        setup,
        &[
            both(
                "SELECT * FROM t WHERE a = 0 AND (b) != (0)",
                X::Err("Unsupported \"!=\" relation: (b) != (0)"),
            ),
            both(
                "SELECT pk1 FROM t2 WHERE token(pk1, pk2) != 0",
                X::Err("Unsupported \"!=\" relation: token(pk1, pk2) != 0"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE token(pk1, pk1) = 0",
                X::Err("The token() function contains duplicate partition key components"),
                X::Err("The token() function must be applied to all partition key components or none of them"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE token(pk1) = 0",
                X::Err("Invalid number of arguments in call to function system.token: 2 required but 1 provided"),
                X::Err("The token() function must be applied to all partition key components or none of them"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE token(v) = 0",
                X::Err("Invalid number of arguments in call to function system.token: 2 required but 1 provided"),
                X::Err("The token() function must be applied to all partition key components or none of them"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE token(pk1, v) = 0",
                X::Err("The token function arguments must be in the partition key order: pk1, pk2"),
                X::Err("The token() function must be applied to all partition key components or none of them"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE token(pk1, pk2, v) = 0",
                X::Err("Invalid number of arguments in call to function system.token: 2 required but 3 provided"),
                X::Err("The token() function must contains only partition key components"),
            ),
            c(
                "SELECT a FROM t WHERE token(a, a) = 0",
                X::Err("Invalid number of arguments in call to function system.token: 1 required but 2 provided"),
                X::Err("The token() function contains duplicate partition key components"),
            ),
            c(
                "SELECT a FROM t WHERE token(b) = 0",
                X::Err("The token function arguments must be in the partition key order: a"),
                X::Err("The token() function must be applied to all partition key components or none of them"),
            ),
            both(
                "SELECT pk1 FROM t2 WHERE token(pk2, pk1) = 0",
                X::Err("The token function arguments must be in the partition key order: pk1, pk2"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE s[2] = 3 ALLOW FILTERING",
                X::Err("Column s cannot be subscripted"),
                X::Err("Column s cannot be used as a map"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE v[2] = 3 ALLOW FILTERING",
                X::Err("Column v is not a map/set/list, cannot be subscripted"),
                X::Err("Column v cannot be used as a map"),
            ),
            c(
                "SELECT pk1 FROM t2 WHERE l[2] = 3 ALLOW FILTERING",
                NONE,
                X::Err("Indexes on list entries (l[index] = value) are not currently supported."),
            ),
            c(
                "SELECT * FROM t3 WHERE u = {a: 1, d: 1h} ALLOW FILTERING",
                NONE,
                X::Err("Non-frozen UDT column 'u' (ud) cannot be restricted by any relation"),
            ),
            c(
                "SELECT * FROM t3 WHERE u > {a: 1, d: 1h} ALLOW FILTERING",
                X::Err("Duration type is unordered for u"),
                X::Err("Slice restrictions are not supported on UDTs containing durations"),
            ),
            c("SELECT * FROM t3 WHERE x > {a: 1, b: 2} ALLOW FILTERING", X::Rows(&[]), X::Err(nonfrozen_x)),
            c("SELECT * FROM t3 WHERE x = {a: 1, b: 2}", X::Err(FILTERING), X::Err(nonfrozen_x)),
            c("SELECT p FROM t3 WHERE x = {a: 1, b: 2} ALLOW FILTERING", X::Rows(&[&["1"]]), X::Err(nonfrozen_x)),
            c("SELECT p FROM t3 WHERE x IN ({a: 1, b: 2}) ALLOW FILTERING", X::Rows(&[&["1"]]), X::Err(nonfrozen_x)),
            c("SELECT p FROM t3 WHERE x = {a: 1} ALLOW FILTERING", NONE, X::Err(nonfrozen_x)),
            c(
                "SELECT * FROM t4 WHERE d > 1h ALLOW FILTERING",
                X::Err("Duration type is unordered for d"),
                X::Err("Slice restrictions are not supported on duration columns"),
            ),
            c(
                "SELECT * FROM t4 WHERE fl > [1h] ALLOW FILTERING",
                X::Err("Duration type is unordered for fl"),
                X::Err("Slice restrictions are not supported on collections containing durations"),
            ),
            c(
                "SELECT * FROM t4 WHERE tu > (1, 1h) ALLOW FILTERING",
                X::Err("Duration type is unordered for tu"),
                X::Err("Slice restrictions are not supported on tuples containing durations"),
            ),
            c(
                "SELECT * FROM t4 WHERE fu > {a: 1, d: 1h} ALLOW FILTERING",
                X::Err("Duration type is unordered for fu"),
                X::Err("Slice restrictions are not supported on UDTs containing durations"),
            ),
            both("SELECT * FROM t4 WHERE d = 1h ALLOW FILTERING", NONE),
        ],
    )
    .await;
}

/// Subscripted restrictions and null keys, values and elements.
#[tokio::test]
async fn subscripts_and_nulls() {
    let setup = "CREATE TABLE t (p int PRIMARY KEY, l list<int>, fl frozen<list<int>>, m map<int,int>, fm frozen<map<int,int>>, s set<int>);\n\
        INSERT INTO t (p, l, fl, m, fm, s) VALUES (1, [10, 20, 30], [10, 20], {1: 2}, {1: 2}, {5})";
    let list_entries =
        X::Err("Indexes on list entries (l[index] = value) are not currently supported.");
    check(
        setup,
        &[
            c(
                "SELECT p FROM t WHERE l[1] = 20 ALLOW FILTERING",
                X::Rows(&[&["1"]]),
                list_entries,
            ),
            c(
                "SELECT p FROM t WHERE l[5] = 20 ALLOW FILTERING",
                NONE,
                list_entries,
            ),
            c(
                "SELECT p FROM t WHERE l[-1] = 20 ALLOW FILTERING",
                NONE,
                list_entries,
            ),
            c(
                "SELECT p FROM t WHERE l[1] > 2 ALLOW FILTERING",
                X::Err("Only EQ relations are supported on map/list entries"),
                list_entries,
            ),
            c(
                "SELECT p FROM t WHERE fl[1] = 20 ALLOW FILTERING",
                X::Err(
                    "List element equality predicates on frozen list column fl are not supported",
                ),
                X::Err("Indexes on list entries (fl[index] = value) are not currently supported."),
            ),
            both(
                "SELECT p FROM t WHERE fm[1] = 2 ALLOW FILTERING",
                X::Err("Map-entry equality predicates on frozen map column fm are not supported"),
            ),
            c(
                "SELECT p FROM t WHERE m[1] > 1 ALLOW FILTERING",
                X::Err("Only EQ relations are supported on map/list entries"),
                X::Err("Only EQ relations are supported on map entries"),
            ),
            both("SELECT p FROM t WHERE m[1] = 2", X::Err(FILTERING)),
            c(
                "SELECT p FROM t WHERE l[1] = 20",
                X::Err(FILTERING),
                list_entries,
            ),
            c(
                "SELECT p FROM t WHERE l[null] = 20 ALLOW FILTERING",
                NONE,
                list_entries,
            ),
            c(
                "SELECT p FROM t WHERE m[null] = 2 ALLOW FILTERING",
                NONE,
                X::Err("Unsupported null map key for column m"),
            ),
            c(
                "SELECT p FROM t WHERE m[1] = null ALLOW FILTERING",
                NONE,
                X::Err("Unsupported null map value for column m"),
            ),
            c(
                "SELECT p FROM t WHERE s CONTAINS null ALLOW FILTERING",
                NONE,
                X::Err("Unsupported null value for column s"),
            ),
            c(
                "SELECT p FROM t WHERE m CONTAINS KEY null ALLOW FILTERING",
                NONE,
                X::Err("Unsupported null value for column m"),
            ),
            c(
                "SELECT p FROM t WHERE m CONTAINS null ALLOW FILTERING",
                NONE,
                X::Err("Unsupported null value for column m"),
            ),
            // Cassandra fails `m[null]` with a server error (java.lang.AssertionError); roll
            // refuses it as invalid instead.
            c(
                "SELECT m[null] FROM t",
                X::Rows(&[&["null"]]),
                X::Err("Invalid null value for element selection"),
            ),
            c(
                "SELECT l[null] FROM t",
                X::Rows(&[&["null"]]),
                X::Err("Element selection is only allowed on sets and maps, but l is a list"),
            ),
            bound(
                "SELECT p FROM t WHERE l[?] = ? ALLOW FILTERING",
                &[V::I(1), V::I(20)],
                X::Rows(&[&["1"]]),
                list_entries,
            ),
        ],
    )
    .await;
}

/// ScyllaDB's multi-column relations start at the first clustering column; Cassandra refuses
/// multi-column slices that would filter.
#[tokio::test]
async fn multi_column_relations() {
    let setup = "CREATE TABLE m (a int, b int, c int, d int, e int, v int, PRIMARY KEY (a, b, c, d, e));\n\
                 CREATE INDEX ON m (v)";
    let skipped = X::Err(
        "Clustering columns may not be skipped in multi-column relations. They should appear in the PRIMARY KEY order",
    );
    let slice = X::Err("Multi-column slice restrictions cannot be used for filtering.");
    check(
        setup,
        &[
            c("SELECT * FROM m WHERE a = 0 AND (c,d) < (2,2) AND v = 0 ALLOW FILTERING", skipped, slice),
            c("SELECT * FROM m WHERE a = 0 AND (d,e) < (2,2) AND b = 1 AND v = 0 ALLOW FILTERING", skipped, slice),
            c("SELECT * FROM m WHERE a = 0 AND b = 1 AND (d,e) < (2,2) AND v = 0 ALLOW FILTERING", skipped, slice),
            c("SELECT * FROM m WHERE a = 0 AND b > 1 AND (d,e) < (2,2) AND v = 0 ALLOW FILTERING", skipped, slice),
            c("SELECT * FROM m WHERE a = 0 AND (b,c) > (1,0) AND (d,e) < (2,2) AND v = 0 ALLOW FILTERING", skipped, slice),
            c("SELECT * FROM m WHERE a = 0 AND (c,d) = (2,2) AND v = 0 ALLOW FILTERING", skipped, NONE),
            c("SELECT * FROM m WHERE a = 0 AND (c,d) < (2,2) ALLOW FILTERING", skipped, slice),
            c("SELECT * FROM m WHERE (c,d) < (2,2) ALLOW FILTERING", skipped, slice),
            c("SELECT * FROM m WHERE a = 0 AND b = 1 AND (c,d) < (2,2) AND v = 0", skipped, NONE),
            c("SELECT * FROM m WHERE a = 0 AND (b) = (1) AND (c) = (2)", skipped, NONE),
            c("SELECT * FROM m WHERE a = 0 AND (b,c) = (1,2) AND (d) > (1)", skipped, NONE),
            c("SELECT * FROM m WHERE a = 0 AND b = 1 AND (c) = (2)", skipped, NONE),
            c(
                "SELECT * FROM m WHERE a = 0 AND (b) = (1) AND c = 2",
                X::Err("Mixing single column relations and multi column relations on clustering columns is not allowed"),
                NONE,
            ),
            both("SELECT * FROM m WHERE a = 0 AND (b) > (1) AND (b) < (3)", NONE),
            both("SELECT * FROM m WHERE a = 0 AND (b, c) > (1, 2) AND (b) < (3)", NONE),
            c("SELECT * FROM m WHERE (c) = (2) ALLOW FILTERING", skipped, NONE),
            c("SELECT * FROM m WHERE a = 0 AND (c) IN ((2)) ALLOW FILTERING", skipped, NONE),
        ],
    )
    .await;
}

/// An index with IN on the clustering key (both plan through the index) or on the partition key.
#[tokio::test]
async fn index_with_in_restrictions() {
    let setup = "CREATE TABLE t (pk int, c1 int, c2 int, c3 int, v int, PRIMARY KEY (pk, c1, c2, c3));\n\
                 CREATE INDEX v_idx_1 ON t (v);\n\
                 INSERT INTO t (pk, c1, c2, c3, v) VALUES (1, 1, 1, 3, 3);\n\
                 INSERT INTO t (pk, c1, c2, c3, v) VALUES (1, 3, 3, 3, 3)";
    let row = X::Rows(&[&["1", "1", "1", "3", "3"]]);
    check(
        setup,
        &[
            both("SELECT * FROM t WHERE pk = 1 AND c1 IN (0, 1, 2) AND c2 = 1 AND v = 3", row),
            both("SELECT * FROM t WHERE pk = 1 AND c1 IN (0, 1, 2) AND v = 3", row),
            both("SELECT * FROM t WHERE pk = 1 AND c1 = 1 AND c2 IN (1, 2) AND v = 3", row),
            both("SELECT * FROM t WHERE pk = 1 AND c1 IN (0, 1, 2) AND c2 = 1 AND v = 3 ALLOW FILTERING", row),
            c(
                "SELECT * FROM t WHERE pk IN (1, 2) AND v = 3",
                X::Err(FILTERING),
                X::Err("Select on indexed columns and with IN clause for the PRIMARY KEY are not supported"),
            ),
        ],
    )
    .await;
}

/// ORDER BY with a single-value IN (an equality) and LIMIT after the post-query sort.
#[tokio::test]
async fn order_by_with_in() {
    let setup = "CREATE TABLE t (a int, b int, c int, d int, PRIMARY KEY (a, b, c));\n\
        INSERT INTO t (a, b, c, d) VALUES (0, 0, 0, 0);\n\
        INSERT INTO t (a, b, c, d) VALUES (0, 1, 0, 3);\n\
        INSERT INTO t (a, b, c, d) VALUES (0, 1, 1, 4);\n\
        CREATE TABLE o (my_id varchar, col1 int, value varchar, PRIMARY KEY (my_id, col1));\n\
        INSERT INTO o (my_id, col1, value) VALUES ('key1', 1, 'a');\n\
        INSERT INTO o (my_id, col1, value) VALUES ('key2', 3, 'c');\n\
        INSERT INTO o (my_id, col1, value) VALUES ('key3', 2, 'b');\n\
        INSERT INTO o (my_id, col1, value) VALUES ('key4', 4, 'd')";
    check(
        setup,
        &[
            bound(
                "SELECT * FROM t WHERE a = ? AND b IN (?) ORDER BY c DESC",
                &[V::I(0), V::I(1)],
                X::Rows(&[&["0", "1", "1", "4"], &["0", "1", "0", "3"]]),
                X::Rows(&[&["0", "1", "1", "4"], &["0", "1", "0", "3"]]),
            ),
            both(
                "SELECT * FROM t WHERE a = 0 AND (b, c) IN ((1, 1)) ORDER BY c DESC",
                X::Rows(&[&["0", "1", "1", "4"]]),
            ),
            both(
                "SELECT col1 FROM o WHERE my_id IN ('key1', 'key2', 'key3') ORDER BY col1 LIMIT 2",
                X::Rows(&[&["1"], &["2"]]),
            ),
        ],
    )
    .await;
}

/// Unset bind values where a value is needed: ScyllaDB names the bind variable, Cassandra the
/// map key, list index, tuple or UDT field or function call.
#[tokio::test]
async fn unset_values() {
    let setup = "CREATE TABLE m (k int PRIMARY KEY, m map<text,text>, l list<int>, s set<int>);\n\
        CREATE TABLE t (k int PRIMARY KEY, t tuple<int, text, double>);\n\
        CREATE INDEX tuple_index ON t (t);\n\
        CREATE TYPE ut (x int, y int);\n\
        CREATE TYPE uo (a frozen<ut>);\n\
        CREATE TABLE u (k int PRIMARY KEY, v frozen<ut>, z frozen<uo>);\n\
        CREATE TABLE i (k int PRIMARY KEY, s text, i int)";
    let u0 = X::Err("Unexpected unset value for bind variable 0");
    const U: V = V::Unset;
    let call = |f: &'static str| -> X {
        X::Err(match f {
            "blobasint" => "Invalid unset value for argument in call to function blobasint",
            "blob_as_int" => "Invalid unset value for argument in call to function blob_as_int",
            _ => "Invalid unset value for argument in call to function token",
        })
    };
    let literal = |kind: &'static str| -> X {
        X::Err(match kind {
            "list" => {
                "Invalid list literal for l: bind variables are not supported inside collection literals"
            }
            "set" => {
                "Invalid set literal for s: bind variables are not supported inside collection literals"
            }
            _ => {
                "Invalid map literal for m: bind variables are not supported inside collection literals"
            }
        })
    };
    check(
        setup,
        &[
            bound(
                "UPDATE m SET m[?] = 'foo' WHERE k = 10",
                &[U],
                u0,
                X::Err("Invalid unset map key"),
            ),
            bound(
                "DELETE m[?] FROM m WHERE k = 10",
                &[U],
                u0,
                X::Err("Invalid unset map key"),
            ),
            bound("UPDATE m SET m['k'] = ? WHERE k = 10", &[U], X::Ok, X::Ok),
            bound(
                "UPDATE m SET l[?] = 1 WHERE k = 10",
                &[U],
                u0,
                X::Err("Invalid unset value for list index"),
            ),
            bound("DELETE l[?] FROM m WHERE k = 10", &[U], X::Ok, X::Ok),
            bound(
                "INSERT INTO m (k, l) VALUES (1, [1, ?])",
                &[U],
                u0,
                literal("list"),
            ),
            bound(
                "INSERT INTO m (k, s) VALUES (1, {1, ?})",
                &[U],
                u0,
                literal("set"),
            ),
            bound(
                "INSERT INTO m (k, m) VALUES (1, {?: 'a'})",
                &[U],
                u0,
                literal("map"),
            ),
            bound(
                "UPDATE m SET l = l + [?] WHERE k = 1",
                &[U],
                u0,
                literal("list"),
            ),
            bound(
                "UPDATE m SET s = s - {?} WHERE k = 1",
                &[U],
                u0,
                literal("set"),
            ),
            bound(
                "INSERT INTO t (k, t) VALUES (0, (3, ?, 2.1))",
                &[U],
                u0,
                X::Err("Invalid unset value for tuple field number 1"),
            ),
            bound(
                "SELECT * FROM t WHERE t = (?, ?, ?)",
                &[V::I(1), U, V::D(1.0)],
                X::Err("Unexpected unset value for bind variable 1"),
                X::Err("Invalid unset value for tuple field number 1"),
            ),
            bound(
                "INSERT INTO u (k, v) VALUES (10, {x: ?, y: ?})",
                &[V::I(1), U],
                X::Err("Unexpected unset value for bind variable 1"),
                X::Err("Invalid unset value for field 'y' of user defined type ut"),
            ),
            bound(
                "INSERT INTO u (k, v, z) VALUES (10, {x: ?, y: ?}, {a: {x: ?, y: ?}})",
                &[V::I(1), V::I(1), V::I(1), U],
                X::Err("Unexpected unset value for bind variable 3"),
                X::Err("Invalid unset value for field 'y' of user defined type ut"),
            ),
            bound(
                "SELECT * FROM i WHERE k = blob_as_int(?)",
                &[U],
                X::Err("Unknown function blob_as_int called"),
                call("blob_as_int"),
            ),
            bound(
                "SELECT * FROM i WHERE k = blobasint(?)",
                &[U],
                u0,
                call("blobasint"),
            ),
            bound(
                "SELECT * FROM i WHERE token(k) >= token(?)",
                &[U],
                u0,
                call("token"),
            ),
            bound(
                "INSERT INTO i (k, i) VALUES (1, blobasint(?))",
                &[U],
                u0,
                call("blobasint"),
            ),
            bound(
                "UPDATE i SET i = blobasint(?) WHERE k = 1",
                &[U],
                u0,
                call("blobasint"),
            ),
            bound(
                "SELECT blobasint(?) FROM i WHERE k = 5",
                &[U],
                X::Rows(&[]),
                call("blobasint"),
            ),
        ],
    )
    .await;
}

/// Null keys in writes and reads: ScyllaDB drops nulls from an `IN` list of several values
/// but takes `IN (x)` as `= x` in writes; Cassandra refuses them everywhere.
#[tokio::test]
async fn null_keys() {
    let setup = "CREATE TABLE t (p text, c text, v int, PRIMARY KEY (p, c))";
    let p = X::Err("Invalid null value in condition for column p");
    let cc = X::Err("Invalid null value in condition for column c");
    check(
        setup,
        &[
            both("DELETE FROM t WHERE p = null", p),
            both("DELETE FROM t WHERE p = null AND c = 'x'", p),
            both("DELETE FROM t WHERE p = 'x' AND c = null", cc),
            both("DELETE FROM t WHERE p IN (null)", p),
            c("DELETE FROM t WHERE p IN ('a', null)", X::Ok, p),
            c("DELETE FROM t WHERE p IN (null, null)", X::Ok, p),
            both("DELETE v FROM t WHERE p = null AND c = 'x'", p),
            both("DELETE FROM t WHERE p = 'x' AND c IN (null)", cc),
            c(
                "UPDATE t SET v = 1 WHERE p IN ('a', null) AND c = 'x'",
                X::Ok,
                p,
            ),
            c(
                "UPDATE t SET v = 1 WHERE p = 'a' AND c IN ('x', null)",
                X::Ok,
                cc,
            ),
            c("SELECT * FROM t WHERE p IN (null)", NONE, p),
            c("SELECT * FROM t WHERE p = 'x' AND c IN (null)", NONE, cc),
            c("SELECT * FROM t WHERE p = null", NONE, p),
            both("INSERT INTO t (p, c) VALUES (null, 'a')", p),
            bound(
                "DELETE FROM t WHERE p IN ?",
                &[V::Null],
                p,
                X::Err("Invalid null value for column p"),
            ),
            bound(
                "SELECT * FROM t WHERE p IN ?",
                &[V::Null],
                NONE,
                X::Err("Invalid null value for column p"),
            ),
            bound(
                "UPDATE t SET v = 1 WHERE p IN ? AND c = 'a'",
                &[V::Null],
                p,
                X::Err("Invalid null value for column p"),
            ),
            bound(
                "SELECT * FROM t WHERE v IN ? ALLOW FILTERING",
                &[V::Null],
                NONE,
                X::Err("Invalid null value for column v"),
            ),
            bound(
                "SELECT * FROM t WHERE p = 'a' AND (c) IN ?",
                &[V::Null],
                NONE,
                X::Err("Invalid null value for IN restriction"),
            ),
            bound("DELETE FROM t WHERE p IN (?)", &[V::Null], p, p),
        ],
    )
    .await;
}

/// Two restrictions on one clustering column, single- and multi-column: Cassandra merges them
/// as multi-column restrictions (the existing one's rules decide), ScyllaDB refuses mixing.
#[tokio::test]
async fn merged_clustering_restrictions() {
    let setup = "CREATE TABLE t (a int, b int, c int, PRIMARY KEY (a, b))";
    let mixing = X::Err(
        "Mixing single column relations and multi column relations on clustering columns is not allowed",
    );
    let equal = X::Err("b cannot be restricted by more than one relation if it includes an Equal");
    let start = X::Err("More than one restriction was found for the start bound on b");
    check(
        setup,
        &[
            c(
                "SELECT * FROM t WHERE a = 0 AND (b) = (0) AND (b) > (0)",
                X::Err("Column \"b\" cannot be restricted by both an equality and an inequality relation"),
                equal,
            ),
            both("SELECT * FROM t WHERE a = 0 AND (b) > (0) AND (b) > (1)", start),
            c("SELECT * FROM t WHERE a = 0 AND (b) > (0) AND b > 1", mixing, start),
            c("SELECT * FROM t WHERE a = 0 AND b > 0 AND (b) > (1)", mixing, start),
            c("SELECT * FROM t WHERE a = 0 AND (b) = (0) AND b > 1", mixing, equal),
            c("SELECT * FROM t WHERE a = 0 AND b = 0 AND (b) > (1)", mixing, equal),
            both(
                "SELECT * FROM t WHERE (a, b) = (0, 0)",
                X::Err("Multi-column relations can only be applied to clustering columns but was applied to: a"),
            ),
        ],
    )
    .await;
}

/// Conditional statements: the old rows ScyllaDB returns for a static condition in a batch
/// (the partition's first row, read for it), nulls in a bound `IN` list, and restrictions
/// that select no clustering row.
#[tokio::test]
async fn conditional_statements() {
    let setup = "CREATE TABLE t (id int, k text, version int static, v text, PRIMARY KEY (id, k));\n\
        INSERT INTO t (id, k, version, v) VALUES (0, 'k1', 2, 'foo');\n\
        INSERT INTO t (id, k, v) VALUES (0, 'k2', 'bar');\n\
        CREATE TABLE e (id int, k text, version int static, v text, PRIMARY KEY (id, k));\n\
        INSERT INTO e (id, version) VALUES (0, 2);\n\
        CREATE TABLE c (p int, c int, v int, PRIMARY KEY (p, c));\n\
        INSERT INTO c (p, c, v) VALUES (1, 1, 1)";
    check(
        setup,
        &[
            both("UPDATE t SET version = 3 WHERE id = 0 IF version = 1", X::Rows(&[&["False", "2"]])),
            c(
                "BEGIN BATCH UPDATE t SET v = 'foobar' WHERE id = 0 AND k = 'k1'; UPDATE t SET v = 'barfoo' WHERE id = 0 AND k = 'k2'; UPDATE t SET version = 3 WHERE id = 0 IF version = 1; APPLY BATCH",
                X::Rows(&[&["False", "0", "k1", "2"], &["False", "0", "null", "2"], &["False", "0", "null", "2"]]),
                X::Rows(&[&["False", "0", "k1", "2"]]),
            ),
            c(
                "BEGIN BATCH UPDATE t SET version = 3 WHERE id = 0 IF version = 1; APPLY BATCH",
                X::Rows(&[&["False", "0", "null", "2"]]),
                X::Rows(&[&["False", "0", "k1", "2"]]),
            ),
            c(
                "BEGIN BATCH UPDATE t SET v = 'x' WHERE id = 0 AND k = 'k9'; UPDATE t SET version = 3 WHERE id = 0 IF version = 1; APPLY BATCH",
                X::Rows(&[&["False", "0", "null", "2"], &["False", "0", "null", "2"]]),
                X::Rows(&[&["False", "0", "k1", "2"]]),
            ),
            c(
                "BEGIN BATCH UPDATE t SET version = 3 WHERE id = 0 IF version = 1; UPDATE t SET v = 'x' WHERE id = 0 AND k = 'k2' IF v = 'nope'; APPLY BATCH",
                X::Rows(&[&["False", "0", "null", "2", "null"], &["False", "0", "k2", "2", "bar"]]),
                X::Rows(&[&["False", "0", "k2", "2", "bar"]]),
            ),
            c(
                "BEGIN BATCH UPDATE e SET v = 'x' WHERE id = 0 AND k = 'k1'; UPDATE e SET version = 3 WHERE id = 0 IF version = 1; APPLY BATCH",
                X::Rows(&[&["False", "0", "null", "2"], &["False", "0", "null", "2"]]),
                X::Rows(&[&["False", "0", "null", "2"]]),
            ),
            bound(
                "UPDATE c SET v = 3 WHERE p = 1 AND c = 1 IF v IN ?",
                &[V::L(&[Some(1), None, Some(3)])],
                X::Rows(&[&["True", "1"]]),
                X::Rows(&[&["True"]]),
            ),
            c(
                "UPDATE c SET v = 1 WHERE p = 1 AND c = 1 AND c = 2 IF v = 1",
                X::Err("Unrestricted clustering key in a conditional update"),
                X::Err("c cannot be restricted by more than one relation if it includes an Equal"),
            ),
            c(
                "DELETE FROM c WHERE p = 1 AND c = 1 AND c = 2 IF EXISTS",
                X::Err("Unrestricted clustering key in a conditional deletion"),
                X::Err("c cannot be restricted by more than one relation if it includes an Equal"),
            ),
            c(
                "UPDATE c SET v = 1 WHERE p = 1 AND c = 1 AND c = 2",
                X::Ok,
                X::Err("c cannot be restricted by more than one relation if it includes an Equal"),
            ),
        ],
    )
    .await;
}

/// `writetime` / `ttl` / `maxwritetime` of collection elements, slices and UDT fields.
#[tokio::test]
async fn element_write_times() {
    let setup = "CREATE TYPE ut (a int, b int);\n\
        CREATE TABLE t (p int PRIMARY KEY, m map<int,int>, fm frozen<map<int,int>>, s set<int>, fs frozen<set<int>>, l list<int>, u ut, fu frozen<ut>, tu tuple<int,int>, v int);\n\
        INSERT INTO t (p, m, fm, s, fs, l, u, fu, tu, v) VALUES (1, {1: 10, 2: 20}, {1: 10}, {1, 2}, {1}, [1], {a: 1}, {a: 1, b: 2}, (1, 2), 5) USING TIMESTAMP 100;\n\
        UPDATE t USING TIMESTAMP 200 SET m[2] = 21, s = s + {3}, u.b = 7 WHERE p = 1;\n\
        INSERT INTO t (p, u) VALUES (2, {a: 5}) USING TIMESTAMP 300;\n\
        CREATE TABLE k (p frozen<map<int,int>> PRIMARY KEY, v int);\n\
        CREATE TABLE k2 (p int, c frozen<ut>, v int, PRIMARY KEY (p, c))";
    let subscript = |k: &'static str| -> X {
        X::Err(match k {
            "w" => "WRITETIME on a subscript is only valid for non-frozen map or set columns",
            _ => "TTL on a subscript is only valid for non-frozen map or set columns",
        })
    };
    let list = X::Err("Element selection is only allowed on sets and maps, but l is a list");
    let scalar = X::Err("Invalid element selection: v is of type int is not a collection");
    check(
        setup,
        &[
            both("SELECT writetime(m[1]) FROM t WHERE p = 1", X::Rows(&[&["100"]])),
            both("SELECT writetime(m[2]), writetime(m[9]) FROM t WHERE p = 1", X::Rows(&[&["200", "null"]])),
            both("SELECT ttl(m[1]), ttl(m[9]) FROM t WHERE p = 1", X::Rows(&[&["null", "null"]])),
            c("SELECT writetime(m[null]) FROM t WHERE p = 1", X::Rows(&[&["null"]]), X::Err("Invalid null value for element selection")),
            c(
                "SELECT writetime(fm[1]), writetime(fm[9]) FROM t WHERE p = 1",
                subscript("w"),
                X::Rows(&[&["100", "null"]]),
            ),
            both(
                "SELECT writetime(s[1]), writetime(s[3]), writetime(s[9]) FROM t WHERE p = 1",
                X::Rows(&[&["100", "200", "null"]]),
            ),
            c("SELECT writetime(fs[1]), writetime(fs[9]) FROM t WHERE p = 1", subscript("w"), X::Rows(&[&["100", "null"]])),
            c("SELECT writetime(l[0]) FROM t", subscript("w"), list),
            c("SELECT ttl(l[0]) FROM t", subscript("t"), list),
            both("SELECT writetime(u.a), writetime(u.b) FROM t WHERE p = 1", X::Rows(&[&["100", "200"]])),
            both("SELECT ttl(u.a), ttl(u.b) FROM t WHERE p = 1", X::Rows(&[&["null", "null"]])),
            both(
                "SELECT p, writetime(u.a), writetime(u.b), ttl(u.a) FROM t WHERE p = 2",
                X::Rows(&[&["2", "300", "null", "null"]]),
            ),
            c(
                "SELECT writetime(fu.a), writetime(fu.b) FROM t WHERE p = 1",
                X::Err("WRITETIME on a field selection is only valid for non-frozen UDT columns"),
                X::Rows(&[&["100", "100"]]),
            ),
            both("SELECT writetime(tu) FROM t WHERE p = 1", X::Rows(&[&["100"]])),
            c(
                "SELECT writetime(v[1]) FROM t",
                X::Err("Column v is not a map/set/list, cannot be subscripted"),
                scalar,
            ),
            c("SELECT ttl(v[1]) FROM t", X::Err("Column v is not a map/set/list, cannot be subscripted"), scalar),
            both("SELECT writetime(v.a) FROM t", X::Err("Invalid field selection: v of type int is not a user type")),
            both(
                "SELECT writetime(tu.a) FROM t",
                X::Err("Invalid field selection: tu of type frozen<tuple<int, int>> is not a user type"),
            ),
            both("SELECT writetime(u.nope) FROM t", X::Err("u of type ut has no field nope")),
            c(
                "SELECT writetime(m), writetime(m[1]), m[1] FROM t WHERE p = 1",
                X::Err("WRITETIME expects an atomic column, but m is a non-frozen collection"),
                X::Rows(&[&["[100, 200]", "100", "10"]]),
            ),
            c(
                "SELECT writetime(u) FROM t WHERE p = 1",
                X::Err("WRITETIME expects an atomic column, but u is a non-frozen collection"),
                X::Rows(&[&["[100, 200]"]]),
            ),
            c("SELECT maxwritetime(m[1]) FROM t WHERE p = 1", X::Err("Unknown function maxwritetime called"), X::Rows(&[&["100"]])),
            c("SELECT writetime(m[1..2]) FROM t WHERE p = 1", X::Code(0x2000), X::Rows(&[&["[100, 200]"]])),
            both("SELECT writetime(m[1]), writetime(m[2]) FROM t WHERE p = 9", NONE),
            c(
                "SELECT writetime(p[1]) FROM k",
                X::Err("WRITETIME is not legal on primary key component p"),
                X::Err("Cannot use selection function writetime on PRIMARY KEY part p"),
            ),
            c(
                "SELECT ttl(c.a) FROM k2",
                X::Err("TTL is not legal on primary key component c"),
                X::Err("Cannot use selection function ttl on PRIMARY KEY part c"),
            ),
        ],
    )
    .await;
}

/// Selectors: casts to the selector's own type, constants (ScyllaDB types and names them),
/// `token()` typed as the partition key, aggregates of bind markers, tuples from strings,
/// infinities and NaN.
#[tokio::test]
async fn selectors() {
    let setup = "CREATE TABLE c (a tinyint PRIMARY KEY, b smallint, c int, d bigint, e float, f double);\n\
        INSERT INTO c (a, b, c, d, e, f) VALUES (1, 2, 3, 4, 5.5, 6.5);\n\
        CREATE TABLE tt (k int, c int, t frozen<tuple<int, text>>, PRIMARY KEY (k, c));\n\
        INSERT INTO tt (k, c, t) VALUES (0, 0, '0:0');\n\
        INSERT INTO tt (k, c, t) VALUES (0, 2, '1');\n\
        INSERT INTO tt (k, c, t) VALUES (0, 3, '1:1\\:1');\n\
        INSERT INTO tt (k, c, t) VALUES (0, 4, '@:1');\n\
        CREATE TABLE tk (k1 uuid, k2 text, PRIMARY KEY ((k1, k2)));\n\
        CREATE TABLE vk (pk vector<int, 2> PRIMARY KEY, v int);\n\
        INSERT INTO vk (pk, v) VALUES ([1, 2], 1);\n\
        CREATE TABLE fl (p int PRIMARY KEY, d double, f float);\n\
        INSERT INTO fl (p, d, f) VALUES (1, infinity, -infinity);\n\
        INSERT INTO fl (p, d, f) VALUES (2, NaN, nan);\n\
        CREATE TABLE ts (p int PRIMARY KEY, t timestamp)";
    let no_infer = |t: &'static str| -> X {
        X::Err(match t {
            "list" => {
                "Cannot infer type for term [1, 2, 3] in selection clause (try using a cast to force a type)"
            }
            "map" => {
                "Cannot infer type for term {1: 2} in selection clause (try using a cast to force a type)"
            }
            "hex" => {
                "Cannot infer type for term 0x01 in selection clause (try using a cast to force a type)"
            }
            _ => {
                "Cannot infer type for term NULL in selection clause (try using a cast to force a type)"
            }
        })
    };
    check(
        setup,
        &[
            both("SELECT CAST(c AS int), CAST(f AS double), CAST(b AS int) FROM c", X::Rows(&[&["3", "6.5", "2"]])),
            both(
                "SELECT * FROM tt",
                X::Rows(&[&["0", "0", "(0, '0')"], &["0", "2", "(1, None)"], &["0", "3", "(1, '1:1')"], &["0", "4", "(None, '1')"]]),
            ),
            c(
                "INSERT INTO tt (k, c, t) VALUES (1, 1, '1:2:3')",
                X::Err("marshaling error: Invalid tuple literal: too many elements. Type frozen<tuple<int, text>> expects 2 but got 3"),
                X::Err("Invalid tuple literal: too many elements. Type frozen<tuple<int, text>> expects 2 but got 3"),
            ),
            c(
                "INSERT INTO tt (k, c, t) VALUES (1, 1, 'x:2')",
                X::Err("marshaling error: Invalid number format 'x'"),
                X::Err("Unable to make int from 'x'"),
            ),
            both(
                "SELECT token(k2, k1) FROM tk",
                X::Err("Type error: k2 cannot be passed as argument 0 of function system.token of type uuid"),
            ),
            both(
                "SELECT token(k1) FROM tk",
                X::Err("Invalid number of arguments in call to function system.token: 2 required but 1 provided"),
            ),
            both("SELECT v FROM vk WHERE token(pk) = token([1, 2])", X::Rows(&[&["1"]])),
            c("SELECT count(1, 2) FROM fl", X::Code(0x0000), X::Err("Invalid number of arguments in call to function system.count: 1 required but 2 provided")),
            c(
                "SELECT count(?) FROM fl",
                X::Err("count() function is only valid when argument types are known"),
                X::Err("Invalid amount of bind variables"),
            ),
            c(
                "SELECT min(?) FROM fl",
                X::Err("min() function is only valid when argument types are known"),
                X::Err("Cannot infer type of argument ? in call to function system.min(any): use type casts to disambiguate"),
            ),
            c("SELECT [1, 2, 3] FROM fl WHERE p = 1", X::Rows(&[&["[1, 2, 3]"]]), no_infer("list")),
            c("SELECT {1: 2} FROM fl WHERE p = 1", X::Rows(&[&["{1: 2}"]]), no_infer("map")),
            c("SELECT 0x01 FROM fl WHERE p = 1", X::Rows(&[&["0x01"]]), no_infer("hex")),
            c("SELECT null FROM fl WHERE p = 1", X::Err("Could not infer type of null"), no_infer("null")),
            c("SELECT [1, 1.5] FROM fl WHERE p = 1", X::Err("Could not infer type of [1, 1.5]"), X::Err("Cannot infer type for term [1, 1.5] in selection clause (try using a cast to force a type)")),
            both("UPDATE fl SET d = -infinity, f = nan WHERE p = 1", X::Ok),
            both("SELECT p, d, f FROM fl WHERE p = 1", X::Rows(&[&["1", "-Infinity", "NaN"]])),
            c("INSERT INTO ts (p, t) VALUES (1, '2011-02-03  Z ')", X::Ok, X::Err("Unable to parse a date/time from '2011-02-03  Z '")),
            c(
                "INSERT INTO ts (p, t) VALUES (2, '2011-02-03 +0000 ')",
                X::Err("marshaling error: unable to parse date '2011-02-03 +0000 ': marshaling error: Unable to parse timezone ' +0000 '"),
                X::Err("Unable to parse a date/time from '2011-02-03 +0000 '"),
            ),
        ],
    )
    .await;
}

/// ScyllaDB's result metadata of constant selectors (probed with PREPARE).
#[tokio::test]
async fn scylla_constant_selector_metadata() {
    let mut h = super::H::new(super::Dialect::Scylla).await;
    h.exec(&format!(
        "{};\nCREATE TABLE ks.fl (p int PRIMARY KEY, d double)",
        super::KS
    ))
    .await;
    let specs = h
        .specs("SELECT [1, 2, 3], {1, 2}, {1: 2}, (1, 'a'), 1, 'a', 1.5, true, 0x01, 1h, 12345678-1234-1234-1234-123456789012, 3000000000, [1.5], {'a': [1]}, [[1, 2]], [(1, 2)], infinity, -nan, [1, 3000000000], {1: 'a', 2: 'b'}, CAST(infinity AS double) FROM ks.fl")
        .await;
    let want: &[(&str, &str)] = &[
        ("[1, 2, 3]", "list<int>"),
        ("{1, 2}", "set<int>"),
        ("{1:2}", "map<int, int>"),
        ("(1, 'a')", "frozen<tuple<int, text>>"),
        ("1", "int"),
        ("'a'", "text"),
        ("1.5", "double"),
        ("true", "boolean"),
        ("01", "blob"),
        ("1h", "duration"),
        ("12345678-1234-1234-1234-123456789012", "uuid"),
        ("3000000000", "bigint"),
        ("[1.5]", "list<double>"),
        ("{'a':[1]}", "map<text, list<int>>"),
        ("[[1, 2]]", "list<list<int>>"),
        ("[(1, 2)]", "list<frozen<tuple<int, int>>>"),
        ("inf", "double"),
        ("nan", "double"),
        ("[1, 3000000000]", "list<bigint>"),
        ("{1:'a', 2:'b'}", "map<int, text>"),
        ("inf", "double"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    assert_eq!(specs, want);
}

/// `similarity_*`: argument checks per dialect (float vectors only, arity, inference of markers
/// and nulls, dimensions) and results.
#[tokio::test]
async fn similarity_functions() {
    let setup = "CREATE TABLE v (pk int PRIMARY KEY, v vector<float, 3>, iv vector<int, 3>, f float, t text);\n\
        INSERT INTO v (pk, v, iv) VALUES (1, [1.1, 2.2, 3.3], [1, 2, 3])";
    let arity = X::Err(
        "Invalid number of arguments for function system.similarity_cosine(vector<float, n>, vector<float, n>)",
    );
    check(
        setup,
        &[
            c(
                "SELECT similarity_cosine(v, [1.1, '2003-05-187T16:20:00.000', 20.25]) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found [1.1, '2003-05-187T16:20:00.000', 20.25]"),
                X::Err("Type error: [1.1, '2003-05-187T16:20:00.000', 20.25] cannot be passed as argument 1 of function system.similarity_cosine of type vector<float, 3>"),
            ),
            c(
                "SELECT similarity_cosine(pk, v) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found pk of type int"),
                X::Err("Function system.similarity_cosine requires a float vector argument, but found argument pk of type int"),
            ),
            c(
                "SELECT similarity_cosine(v, iv) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found iv of type vector<int, 3>"),
                X::Err("Function system.similarity_cosine requires a float vector argument, but found argument iv of type vector<int, 3>"),
            ),
            c(
                "SELECT similarity_cosine(['a', 'b'], ['c', 'd']) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found ['a', 'b'] of type frozen<list<text>>"),
                X::Err("Type error: ['a', 'b'] cannot be passed as argument 0 of function system.similarity_cosine of type vector<float, 2>"),
            ),
            c(
                "SELECT similarity_cosine(v, 5) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found 5 of type int"),
                X::Err("Function system.similarity_cosine requires a float vector argument, but found argument 5 of type int"),
            ),
            c(
                "SELECT similarity_cosine(v, 'a') FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found 'a' of type text"),
                X::Err("Function system.similarity_cosine requires a float vector argument, but found argument 'a' of type ascii"),
            ),
            both("SELECT similarity_cosine(v) FROM v", arity),
            both("SELECT similarity_cosine(v, v, v) FROM v", arity),
            both("SELECT similarity_cosine(v, null) FROM v", X::Rows(&[&["null"]])),
            c(
                "SELECT similarity_cosine(null, null) FROM v",
                X::Err("Cannot infer type of argument null for function system.similarity_cosine(vector<float, n>, vector<float, n>)"),
                X::Err("Cannot infer type of argument NULL in call to function system.similarity_cosine(vector<float, n>, vector<float, n>): use type casts to disambiguate"),
            ),
            c(
                "SELECT similarity_cosine(?, ?) FROM v",
                X::Err("Cannot infer type of argument ? for function system.similarity_cosine(vector<float, n>, vector<float, n>)"),
                X::Err("Cannot infer type of argument ? in call to function system.similarity_cosine(vector<float, n>, vector<float, n>): use type casts to disambiguate"),
            ),
            c(
                "SELECT similarity_cosine(v, [1.0, 2.0]) FROM v",
                X::Err("All arguments must have the same vector dimensions, but found vector<float, 3> and vector<float, 2>"),
                X::Err("All arguments must have the same vector dimensions"),
            ),
            c(
                "SELECT similarity_cosine({1.0, 2.0, 3.0}, v) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found {1.0, 2.0, 3.0} of type frozen<set<double>>"),
                X::Err("Function system.similarity_cosine requires a float vector argument, but found argument {1.0, 2.0, 3.0} of type frozen<set<double>>"),
            ),
            c(
                "SELECT similarity_cosine((1, 2, 3), v) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found (1, 2, 3) of type frozen<tuple<int, int, int>>"),
                X::Err("Function system.similarity_cosine requires a float vector argument, but found argument (1, 2, 3) of type frozen<tuple<int, int, int>>"),
            ),
            c(
                "SELECT similarity_cosine(1.5, v) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found 1.5 of type double"),
                X::Err("Function system.similarity_cosine requires a float vector argument, but found argument 1.5 of type double"),
            ),
            c(
                "SELECT similarity_cosine({1: 2}, v) FROM v",
                X::Err("Function system.similarity_cosine requires a float vector argument, but found {1:2} of type frozen<map<int, int>>"),
                X::Code(0x2200),
            ),
            both("SELECT similarity_cosine(v, v) FROM v", X::Rows(&[&["1"]])),
            c("SELECT similarity_cosine([1, 2, 3], [4, 5, 6]) FROM v", X::Rows(&[&["0.987316"]]), X::Rows(&[&["0.987316"]])),
            both("SELECT similarity_dot_product(v, [1.0, 2.0, 3.0]) FROM v", X::Rows(&[&["8.2"]])),
        ],
    )
    .await;
}

/// ScyllaDB computes similarities in double precision (0.98731595 for these vectors, as
/// probed), Cassandra in float (0.9873159).
#[tokio::test]
async fn similarity_precision() {
    for (d, want) in [
        (super::Dialect::Scylla, 0.987_315_95_f32),
        (super::Dialect::Cassandra, 0.987_315_9_f32),
    ] {
        let mut h = super::H::new(d).await;
        h.exec(&format!(
            "{};\nCREATE TABLE ks.v (pk int PRIMARY KEY, a vector<float, 3>, b vector<float, 3>);\n\
             INSERT INTO ks.v (pk, a, b) VALUES (1, [1, 2, 3], [4, 5, 6])",
            super::KS
        ))
        .await;
        let value = |r: crate::Response| {
            let b = r
                .rows()
                .and_then(|r| r.rows[0][0].clone())
                .expect("a value");
            f32::from_be_bytes([b[0], b[1], b[2], b[3]])
        };
        let got = value(h.ok("SELECT similarity_cosine(a, b) FROM ks.v").await);
        assert_eq!(got.to_bits(), want.to_bits(), "{d}: {got}");
        // ScyllaDB's opposite vectors are exactly 0 (one square root of the norms' product).
        if d.is_scylla() {
            let q = "SELECT similarity_cosine([0.45, 0.09, 0.01, 0.2, 0.11], [-0.45, -0.09, -0.01, -0.2, -0.11]) FROM ks.v";
            assert_eq!(value(h.ok(q).await), 0.0, "{d}");
        }
    }
}

/// Schema statements: UDT rules per dialect (nested non-frozen collections, non-frozen UDT
/// fields, types used in a partition key, keyspace-qualified type names), the adder message,
/// and ScyllaDB's `DROP c USING TIMESTAMP ?`.
#[tokio::test]
async fn schema_statements() {
    let setup = "CREATE TYPE mt (a int);\n\
        CREATE TYPE t2 (bar int, foo list<int>);\n\
        CREATE TABLE ok2 (k int PRIMARY KEY, v frozen<t2>);\n\
        CREATE TYPE t1 (v1 int);\n\
        CREATE TABLE zz (pk frozen<t1>, val int, PRIMARY KEY (pk));\n\
        CREATE TABLE aa (pk frozen<list<frozen<t1>>>, val int, PRIMARY KEY (pk));\n\
        CREATE TABLE mm (p1 int, p2 frozen<t1>, PRIMARY KEY ((p1, p2)));\n\
        CREATE TYPE t9 (v1 int);\n\
        CREATE TABLE cc (p int, c frozen<t9>, PRIMARY KEY (p, c));\n\
        CREATE TABLE an (pk int PRIMARY KEY, a int, b text);\n\
        CREATE TABLE dr (p int PRIMARY KEY, a int, b int);\n\
        INSERT INTO dr (p, a, b) VALUES (1, 1, 1) USING TIMESTAMP 100";
    check(
        setup,
        &[
            both(
                "CREATE TABLE wrong (k int PRIMARY KEY, v t2)",
                X::Err("Non-frozen UDTs with nested non-frozen collections are not supported"),
            ),
            c(
                "ALTER TABLE ok2 ADD w t2",
                X::Ok,
                X::Err("Non-frozen UDTs with nested non-frozen collections are not supported for column w"),
            ),
            both(
                "SELECT field_types FROM system_schema.types WHERE keyspace_name = 'ks' AND type_name = 't2'",
                X::Rows(&[&["['int', 'list<int>']"]]),
            ),
            c(
                "CREATE TABLE wrong (k int PRIMARY KEY, v map<int, mt>)",
                X::Err("Non-frozen user types or collections are not allowed inside collections: map<int, ks.mt>"),
                X::Err("Non-frozen UDTs are not allowed inside collections: map<int, mt>"),
            ),
            c(
                "CREATE TYPE wrong (a int, b mt)",
                X::Err("A user type cannot contain non-frozen user type fields"),
                X::Err("A user type cannot contain non-frozen UDTs"),
            ),
            c("CREATE TYPE ut1 (a int)", X::Ok, X::Ok),
            c("ALTER TYPE ut1 ADD b mt", X::Ok, X::Err("A user type cannot contain non-frozen UDTs")),
            c(
                "CREATE TABLE okt (k int PRIMARY KEY, v mt)",
                X::Ok,
                X::Ok,
            ),
            c(
                "DROP TYPE mt",
                X::Err("Cannot drop user type ks.mt as it is still used by user type ut1"),
                X::Err("Cannot drop user type 'ks.mt' as it is still used by tables okt"),
            ),
            c(
                "ALTER TYPE t1 ADD v2 int",
                X::Err("Cannot add new field to type ks.t1 because it is used in the partition key column p2 of table ks.mm"),
                X::Err("Cannot add new field v2 of type int to user type ks.t1 as the type is being used in partition key by the following tables: ks.zz, ks.aa, ks.mm"),
            ),
            c(
                "ALTER TYPE t1 RENAME v1 TO v1_renamed",
                X::Err("Cannot add new field to type ks.t1 because it is used in the partition key column p2 of table ks.mm"),
                X::Ok,
            ),
            both("ALTER TYPE t9 ADD v2 int", X::Ok),
            both("ALTER TYPE t9 RENAME v1 TO v3", X::Ok),
            c(
                "UPDATE an SET a = a + 1, b = b + 'fail' WHERE pk = 1",
                X::Err("Invalid operation (b = b + 'fail') for non counter column b"),
                X::Err("Invalid operation (a = a + 1) for non counter column a"),
            ),
            c(
                "UPDATE an SET a = a + 1 WHERE pk = 1",
                X::Err("SET with a column expression (e.g. col = col + 1) requires an LWT condition (e.g., IF col != NULL or IF EXISTS) to ensure atomic read-before-write"),
                X::Err("Invalid operation (a = a + 1) for non counter column a"),
            ),
            bound("ALTER TABLE dr DROP a USING TIMESTAMP ?", &[V::B(200)], X::Ok, SYNTAX),
            c("SELECT * FROM dr", X::Rows(&[&["1", "1"]]), X::Rows(&[&["1", "1", "1"]])),
        ],
    )
    .await;
}

/// ScyllaDB's `strict_allow_filtering`: clustering restrictions over a key range warn by
/// default, fail with `true` (the suites' setting) and pass silently with `false`; Cassandra
/// always requires ALLOW FILTERING.
#[tokio::test]
async fn strict_allow_filtering() {
    use crate::config::StrictAllowFiltering as S;
    const WARN: &str =
        "This query should use ALLOW FILTERING and will be rejected in future versions.";
    check(
        "CREATE TABLE t (k int, c int, v int, PRIMARY KEY (k, c))",
        &[c(
            "SELECT * FROM t WHERE c = 2",
            X::Warn(&[], WARN),
            X::Err(FILTERING),
        )],
    )
    .await;
    for (mode, want) in [(S::True, Some(FILTERING)), (S::False, None)] {
        let store = nosql_storage::store::Store::start(
            nosql_storage::test_objects("engine"),
            super::small_store_config(),
        )
        .await
        .unwrap();
        let handle = store.open("db", Default::default()).await.unwrap();
        let mut cfg = crate::DbConfig::new("db", super::Dialect::Scylla);
        cfg.settings.strict_allow_filtering = mode;
        let db = crate::Database::open(cfg, handle).await.unwrap();
        let s = db.connect(Default::default());
        let mut h = super::H { db, s, store };
        h.exec(&format!(
            "{};\nCREATE TABLE ks.t (k int, c int, PRIMARY KEY (k, c))",
            super::KS
        ))
        .await;
        for q in [
            "SELECT * FROM ks.t WHERE c = 2",
            "SELECT * FROM ks.t WHERE c > 2",
            "SELECT * FROM ks.t WHERE (c) = (2)",
        ] {
            match (h.run(q).await, want) {
                (Err(e), Some(m)) => assert_eq!(e.message, m, "{mode:?}: {q}"),
                (Ok(r), None) => assert!(r.warnings.is_empty(), "{mode:?}: {q}"),
                (r, _) => panic!("{mode:?}: {q}: {:?}", r.map(|r| r.warnings)),
            }
        }
        assert!(
            h.run("SELECT * FROM ks.t WHERE c = 2 ALLOW FILTERING")
                .await
                .is_ok()
        );
    }
}

/// Functions: ScyllaDB's timeuuid range checks, decimal averages, integer widening of function
/// results, `counterasblob` of a counter column, `set_intersection` of a literal.
#[tokio::test]
async fn functions() {
    let setup = "CREATE TABLE t (p int PRIMARY KEY, b bigint, i int, v varint, dc decimal);\n\
        INSERT INTO t (p) VALUES (1);\n\
        CREATE TABLE d (p int, c int, dc decimal, PRIMARY KEY (p, c));\n\
        INSERT INTO d (p, c, dc) VALUES (1, 1, 2);\n\
        INSERT INTO d (p, c, dc) VALUES (1, 2, 3);\n\
        CREATE TABLE ct (p int PRIMARY KEY, c counter);\n\
        UPDATE ct SET c = c + 3 WHERE p = 1;\n\
        CREATE TABLE s (id int PRIMARY KEY, vals2 set<frozen<map<text, int>>>);\n\
        INSERT INTO s (id, vals2) VALUES (1, {{'cc': 3, 'dd': 4}, {'aa': 1}})";
    let int_to_bigint = "Type error: cannot assign result of function system.blobasint (type int) to b (type bigint)";
    check(
        setup,
        &[
            both(
                "SELECT tounixtimestamp(mintimeuuid(-12219292800000)) FROM t",
                X::Rows(&[&["-12219292800000"]]),
            ),
            c("SELECT tounixtimestamp(mintimeuuid(-12219292800001)) FROM t", X::Code(0x1400), X::Ok),
            c("SELECT tounixtimestamp(maxtimeuuid(103072857660684)) FROM t", X::Code(0x0000), X::Ok),
            c("SELECT tounixtimestamp(mintimeuuid(-127511443260684)) FROM t", X::Code(0x0000), X::Ok),
            both("SELECT tounixtimestamp(mintimeuuid(103072857660683)) FROM t", X::Rows(&[&["103072857660683"]])),
            c("SELECT avg(dc) FROM d WHERE p = 1", X::Rows(&[&["2"]]), X::Rows(&[&["2"]])),
            c("UPDATE t SET b = blobasint(0x00000002) WHERE p = 1", X::Ok, X::Err(int_to_bigint)),
            c("SELECT b FROM t WHERE p = 1", X::Rows(&[&["2"]]), X::Rows(&[&["null"]])),
            c(
                "UPDATE t SET b = (bigint)blobasint(0x00000003) WHERE p = 1",
                X::Ok,
                X::Err("Cannot cast value blobasint(0x00000003) to type bigint"),
            ),
            both("UPDATE t SET v = blobasbigint(0x0000000000000004) WHERE p = 1", X::Ok),
            both(
                "UPDATE t SET i = blobasbigint(0x0000000000000001) WHERE p = 1",
                X::Err("Type error: cannot assign result of function system.blobasbigint (type bigint) to i (type int)"),
            ),
            both(
                "UPDATE t SET dc = blobasint(0x00000001) WHERE p = 1",
                X::Err("Type error: cannot assign result of function system.blobasint (type int) to dc (type decimal)"),
            ),
            both("SELECT blobasbigint(counterasblob(c)) FROM ct", X::Rows(&[&["3"]])),
            c(
                "SELECT bigintasblob(c) FROM ct",
                X::Rows(&[&["0x0000000000000003"]]),
                X::Err("Type error: c cannot be passed as argument 0 of function system.bigintasblob of type bigint"),
            ),
            c(
                "SELECT set_intersection(vals2, {{'cc': 3, 'dd': 4}, {'cc': 3, 'dd': 5}}) FROM s WHERE id = 1",
                X::Rows(&[&["{{'cc': 3, 'dd': 4}}"]]),
                X::Err("Unknown function 'set_intersection'"),
            ),
        ],
    )
    .await;
}

/// PREPARE metadata: ScyllaDB's shared `:name` markers route every partition key column they
/// bind, its `NOT IN(c)` names, and Cassandra's `[selection]` markers.
#[tokio::test]
async fn prepared_metadata() {
    for d in super::BOTH {
        let mut h = super::H::new(d).await;
        h.exec(&format!(
            "{};\nCREATE TABLE ks.t (p int, c int, v int, PRIMARY KEY (p, c));\n\
             CREATE TABLE ks.t4 (p1 int, p2 int, p3 int, p4 int, PRIMARY KEY ((p1, p2, p3, p4)))",
            super::KS
        ))
        .await;
        let meta = |p: &crate::response::Prepared| {
            (
                p.bind.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
                p.pk_indexes.clone(),
            )
        };
        let p =
            h.s.prepare("SELECT p FROM ks.t WHERE c = :x AND p = :x", None)
                .await
                .unwrap();
        let scylla = d.is_scylla();
        assert_eq!(
            meta(&p),
            if scylla {
                (vec!["x".to_string()], vec![0])
            } else {
                (vec!["x".into(), "x".into()], vec![1])
            },
            "{d}"
        );
        let p =
            h.s.prepare(
                "SELECT * FROM ks.t4 WHERE p1 = :x AND p2 = :y AND p3 = :x AND p4 = :y",
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            meta(&p).1,
            if scylla {
                vec![0, 1, 0, 1]
            } else {
                vec![0, 1, 2, 3]
            },
            "{d}"
        );
        if scylla {
            let p =
                h.s.prepare("SELECT * FROM ks.t WHERE p = ? AND c NOT IN ?", None)
                    .await
                    .unwrap();
            assert_eq!(meta(&p).0, vec!["p".to_string(), "NOT IN(c)".into()]);
            let p =
                h.s.prepare("SELECT * FROM ks.t WHERE p = ? AND c NOT IN (?, ?)", None)
                    .await
                    .unwrap();
            assert_eq!(
                meta(&p).0,
                vec![
                    "p".to_string(),
                    "value(NOT IN(c))".into(),
                    "value(NOT IN(c))".into()
                ]
            );
        }
        let p =
            h.s.prepare("SELECT blobasint(?) FROM ks.t", None)
                .await
                .unwrap();
        assert_eq!(
            meta(&p).0,
            vec![
                if scylla {
                    "arg0(system.blobasint)"
                } else {
                    "[selection]"
                }
                .to_string()
            ],
            "{d}"
        );
    }
}
