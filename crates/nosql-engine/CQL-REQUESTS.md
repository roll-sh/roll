# Requests from nosql-engine to nosql-cql

Parser behaviour the engine's reference tests found. Each has the engine-side workaround.

## 1. `-Infinity` in a VALUES list (ScyllaDB) — done

Fixed in `nosql-cql` (VALUES take `- NaN` / `- Infinity` like other terms; test in
`tests/statements.rs`); the statements harness no longer binds the value.

Original report:

`INSERT INTO s.ty (pk, f) VALUES (2, -Infinity)` fails with `line 1:68 : Unexpected 'Infinity'`
in the ScyllaDB dialect; `scylla:2026.3.3` accepts it (the statements capture inserts it and reads
back `-inf`). Workaround: `tests/captured_statements_harness.rs` binds the value instead
(`PARSER_WORKAROUNDS`).

## 2. Nesting depth

`SELECT * FROM e.t WHERE pk = ((( … 203 levels … 1 … )))`:

- Cassandra 5.0.9 parses and runs it (OK, Rows); our parser fails with `StackOverflowError`.
- ScyllaDB answers `SyntaxException` with the message `expression nested too deeply`.

Case `deep_nesting` in `captured_cases.rs`, skipped in the harness until fixed.

## 3. ScyllaDB service levels named `default` — done

Fixed in `nosql-cql` (`default` is a service level name in the ScyllaDB dialect; test in
`tests/statements.rs`); `attach_default` runs in the auth harness again.

Original report:

`CREATE SERVICE LEVEL default`, `ATTACH SERVICE LEVEL default TO r`, `ALTER SERVICE LEVEL default
WITH shares = 5`, `DROP SERVICE LEVEL default` and `LIST SERVICE LEVEL default` fail with `line 1:21
no viable alternative at input 'default'`; `scylla:2026.3.3` parses them (and answers `The default
service level, default, already exists and cannot be created` / `… cannot be attached to a role. …`
/ `… cannot be altered` / `… cannot be dropped`, or an empty result for LIST). The engine already
answers them (`auth/service_levels.rs`); case `attach_default` is skipped in
`tests/captured_auth_harness.rs` (`NOT_IN_AUTH`).

## 4. ScyllaDB's indexing permissions

`GRANT VECTOR_SEARCH_INDEXING ON ALL KEYSPACES TO r` (also `TEXT_SEARCH_INDEXING`,
`PATTERN_SEARCH_INDEXING`) fails with `line 1:29 : Syntax error`; ScyllaDB 2026.3 accepts them on
`ALL KEYSPACES` only (`GRANT VECTOR_SEARCH_INDEXING ON KEYSPACE ks …` →
`Resource <keyspace ks> does not support any of the requested permissions.`). Needs
`Permission` variants (excluded from `ALL`: `LIST ALL PERMISSIONS` doesn't show them). Cases
`grant_vector_search*` are skipped in `tests/captured_auth_harness.rs`.

## 3. Bind markers in `CREATE MATERIALIZED VIEW ... WHERE`

`CREATE MATERIALIZED VIEW ks.v AS SELECT pk, ck FROM ks.b WHERE pk IS NOT NULL AND ck = ? PRIMARY KEY (ck, pk)`
fails to parse (`line 1:93 no viable alternative at input '?'`). Both references parse it and reject it
semantically: ScyllaDB with `Cannot use query parameters in CREATE MATERIALIZED VIEW statements`,
Cassandra (views enabled) with `Bind variables are not allowed in CREATE MATERIALIZED VIEW statements`
(`crate::views::ddl` raises both once the statement parses). Case `mv_bind_marker` in
`captured_derived.rs`, skipped in the harness until fixed.

## 5. `OR REPLACE` with `IF NOT EXISTS` in ScyllaDB's `CREATE FUNCTION` — done

Fixed in `nosql-cql` (`line 1:27 no viable alternative at input 'IF'`; tests in
`tests/statements.rs`, `tests/udf.rs` and the captured `lua_ddl` battery).

Original report:

`CREATE OR REPLACE FUNCTION IF NOT EXISTS ks.f(x int) RETURNS NULL ON NULL INPUT RETURNS int
LANGUAGE lua AS 'return x'` parses in the scylla dialect; `scylla:2026.3.3` rejects it with `line
1:27 no viable alternative at input 'IF'` (its `CREATE OR REPLACE AGGREGATE IF NOT EXISTS` does
parse, and answers "User defined functions are disabled …"). Cassandra parses both and answers
`Cannot use both 'OR REPLACE' and 'IF NOT EXISTS' directives` (`crate::udf::ddl`). Pinned in
`tests/udf.rs` (`disabled_statements`) until fixed.

## 6. Keyspace-qualified functions named `token` / `count` (ScyllaDB) — done

`SELECT ks.token(v) FROM ks.t` (a user function named `token`) failed with `line 1:7 no viable
alternative at input 'ks'` in the ScyllaDB dialect; `scylla:2026.3.3` calls the function. Fixed in
`nosql-cql` (test in `tests/statements.rs`; captured case `call_token_udf`).
