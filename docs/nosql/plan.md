# roll nosql — plan and status

Phases overlap; agents work on independent crates in parallel. "Done" means:

- tests cover everything in it: every public function, statement, protocol path, error path,
  edge case and dialect difference, verified against the reference where it is observable
- clippy passes
- the relevant conformance tests pass in both dialects

## Phase 1: foundations (independent crates)

- [x] `nosql-types`: every type, ordering, serialisation, validation, literals, JSON, tokens, timeuuids
- [x] `nosql-proto`: v3/v4/v5 frames and segments, compression, every message (both directions), client
- [x] `nosql-cql`: the full grammar of both dialects, AST, ANTLR-shaped errors
- [x] `nosql-storage`: memtables, SSTables, merge/reconcile, compaction, commit log shipping, manifests, branches, PITR, caches
- [x] Reference captures (`docs/nosql/reference/`) and the conformance runner (`scripts/nosql-conformance.sh`) with the reference baselines

## Phase 2: engine and server

- [x] Engine core: schema and DDL, `system_schema`, `system.local`/`peers`, DML, SELECT planning, paging (68 suites incl. ~500 captured error cases, both dialects)
- [x] Server: CQL listener, sessions, auth handshake, prepared cache, events, single mode, shard-aware port, TLS/SNI, PROXY protocol, HTTP data API, ScyllaDB REST subset (60 tests)
- [x] Differential harness `tests/compat.rs` and the Rust driver tests (harness grows with each subsystem)
- [x] Types in depth: collections, UDTs, tuples, vectors, durations, counters, JSON, casts, functions
- [x] LWT, batches, TTL/timestamps (LWT: 409 captured steps on both dialects)
- [x] Secondary indexes (both kinds), SAI, vector ANN (exact), materialized views, CDC (616 captured steps)
- [x] Auth: roles, permissions, service levels, network permissions (559 captured steps; masking enforcement pending)
- [x] DESCRIBE (57 captured statements byte for byte), tracing, virtual tables
- [x] Lua UDFs/UDAs, Cassandra 5 math/collection/masking functions, column masking, platform masking policy
- [x] Lua 5.4 for UDFs (vendored `nosql-lua`), UDF/UDA/masking/views verified on the feature-enabled references (1307 + 587 + 447 captured cases)
- [x] ScyllaDB REST API subset (55 endpoints compared live)

## Phase 3: serverless

- [x] Multi-tenant `Node`: routing (SNI, `role@db`), idle unload, wake on connect, TLS, PROXY protocol
- [x] `nosql-control`: store, API, metering, pricing, live CQL import from Cassandra/ScyllaDB (row-for-row incl. writetime/TTL)
- [x] `roll nosql` CLI with the built-in cqlsh (golden-identical to both references' cqlsh)
- [x] rolld wiring (`--nosql-*`, single mode, `--nosql-only`)
- [x] Dashboard pages (`/dashboard/nosql`: list, overview, branches + diff, schema, data, console, imports, usage, activity)
- [x] Branches, reset, expiry, restore, schema diff, read-only branches
- [x] Settings applied without reload, client registry, recent warnings, read-only enforced in the engine, tracing end to end

## Phase 4: Alternator

- [x] Engine-independent layer: wire format, SigV4, compression, every expression language, all operations over a `Backend` trait with an in-memory backend (golden replay: 7542/7620 responses identical)
- [x] `Backend` on the engine (`alternator_T.T` + `:attrs`), LWT writes, GSI/LSI views, Streams from CDC, TTL, internal tables, credentials and permissions from roles, metrics, served by nosql-server and rolld (golden replay on the engine: 7616/7620, the other 4 are ScyllaDB hash orders)

## Phase 5: getting data in, local dev

- [x] Import from a live cluster over CQL (scheduled mirrors)
- [x] Import from Cassandra/ScyllaDB SSTables in S3 (nb, oa, da, me, ms, mt; snapshots, Manager and Medusa layouts — the last two tested on doc-built layouts only)
- [x] `roll/nosql` Docker image (159 ms to ready, 72 MB), Testcontainers (Python, Java, Go, Node), golang-migrate, cassandra-stress, scylla-bench, CI recipes (`local-dev.md`)
- [x] Driver suites: Python (cassandra-driver, scylla-driver), Java (DataStax 4.x, ScyllaDB fork), Go (gocql, ScyllaDB fork), Node — 47 checks each, matching the references

## Phase 6: hardening

- [x] Coverage audit (`coverage.md`): S3 (SeaweedFS) and TLS end to end, failure injection, loop timers; 5 bugs fixed
- [x] Client-certificate authentication (Cassandra's `MutualTls*Authenticator`, ScyllaDB's `Certificate*Authenticator`) and TLS from a mounted cassandra.yaml / scylla.yaml in the image
- [x] Cassandra's tombstone warning (`tombstone_warn_threshold`): `ReadCommand.toCQLString` ported, 84 query shapes × every page captured on cassandra:5.0.9 and replayed byte for byte (memtables and SSTables)
- [x] Remaining gaps: the 25 + 5 cqlpy and 36 alternator known failures, down to the 5 Cassandra tests that run Java UDF bodies (no JVM by design): audit logging, WASM UDFs, Alternator's live options through CQL and its expression cache, tombstone-limited Alternator pages, tablet resizes of CDC streams, Alternator tracing, purging against memtable data
- [x] Conformance baselines at zero unexplained failures in both dialects (the 5 Cassandra Java UDF tests are explained: no JVM by design)
- [x] Benchmarks (`docs/benchmarks/nosql.md`), memory budgets, CI workflow, README section, AGENTS.md section

## Conformance status

References themselves (suites pinned to `scylla-2026.3.3`): cqlpy on ScyllaDB 2009 pass / 2532,
cqlpy on Cassandra 1214 pass / 2532, alternator on ScyllaDB 1253 pass / 1426. roll must pass
every test its reference passes.

| suite | scylla dialect | cassandra dialect |
|---|---|---|
| ScyllaDB `test/cqlpy` | 2014 pass / 2532; all 2009 of the reference's (0 known failures) | 1217 pass / 2532; 1209 of the reference's 1214 (5 known failures: Java UDF bodies, which roll doesn't run) |
| ScyllaDB `test/alternator` | 1253 pass / 1426; all 1253 of the reference's (0 known failures) | n/a |
| differential scripts | — | — |

Numbers are from the full runs of 2026-10-21 with `--compare` (cqlpy scylla 417 s, cqlpy
cassandra 301 s, alternator 420 s), after the known-failure work (conformance-triage.md, "Known
failures to zero"). Before it: the runs of 2026-10-20 22:00 (scylla 1989, cassandra 1218, alternator
1217 pass), after the guardrails, table options, per-row TTL, `MUTATION_FRAGMENTS()`, USING
TIMEOUT, service level, DESCRIBE and metrics work. Strict XPASS (a test the suite marks as a ScyllaDB bug, passing on
roll) is accepted when the reference XFAILs it: roll doesn't copy those bugs. Earlier: the second
full runs (2026-10-20, after the core query-semantics fixes;
first run: 1271 / 1087 pass). The 3 scylla regressions were `compact_storage_test.py::testCompactCollections`
and `test_secondary_index_properties.py::test_create_index_obsolete_view_properties` (in progress
elsewhere) and a cosine rounding error (fixed since). A later scylla run with the derived-data
fixes passed 1877 (1872 of the reference's 2009); see conformance-triage.md. The known failures are in
`crates/nosql-server/tests/conformance/roll-known-failures-<dialect>.txt` and may only shrink;
root causes and owners are in [conformance-triage.md](conformance-triage.md).
