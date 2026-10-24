# roll nosql — coverage audit (Phase 6)

What each crate's tests cover, the untested paths the audit found, which of them now have
tests, and which are left on purpose. `cargo llvm-cov` isn't installed on the dev machine, so
this is a reasoned audit: public functions, routes, opcodes, commands and error paths, checked
against `src/**/tests`, `tests/*.rs` and the differential, driver, tool and conformance suites.

Re-audit after large changes, and add a line here when you leave a path untested on purpose.

## How the suites run

- `pnpm test:nosql` (`scripts/test-nosql.sh`) runs everything below. CI runs it in
  `.github/workflows/nosql.yml`.
- **S3.** Every suite built on `nosql_storage::test_objects` uses S3 under a unique prefix when
  `ROLL_E2E_S3_BUCKET` is set (with `ROLL_E2E_S3_ENDPOINT` and the AWS credentials). That covers
  nosql-storage, nosql-engine, nosql-alternator, nosql-server, nosql-control and roll-cli's
  `nosql_api` and `cqlsh_roll`. `rolld/tests/nosql.rs` passes the same bucket to rolld as
  `ROLL_S3_BUCKET` / `ROLL_S3_PREFIX`. `import_sstables.rs::s3_backup` uploads a fixture to
  the bucket and imports it from `s3://`. The CI `s3` job runs all of them on SeaweedFS.
- **Result on SeaweedFS (2026-10-21).** These all passed:
  - nosql-storage lib: 108
  - nosql-server: `node`, `rest`, `http`, `alternator`, `drivers` and `protocol`
  - nosql-alternator: every suite
  - nosql-control: `api`, `branches`, `imports`, `import_sstables` (including `s3_backup`) and
    `loops`
  - rolld: `nosql`
  - roll-cli: `nosql_api` and `cqlsh_roll`

  No S3-specific failure was found. SeaweedFS (`server -s3`) capped at 512 MB was OOM-killed
  after the whole sequence: it reached 499 MB in `import_sstables`. Give it more memory, or
  restart it between crates. The CI job has no cap.
- **TLS** is now tested end to end in five places:
  - `rolld/tests/nosql.rs::tls_sni_and_proxy_protocol`: rolld in hosted mode with a
    certificate, `--nosql-domain` and `--nosql-proxy-protocol`. It checks:
    - CQL through a PROXY v1 load balancer by SNI, on both CQL ports;
    - that `system.clients` shows the PROXY address and the TLS session;
    - routing by `user@db` for an unknown server name;
    - that plain text and connections without the PROXY header are refused;
    - Alternator over HTTPS and HTTP routed by `<id>.alternator.<domain>`;
    - that the API's endpoints and credentials work as given;
    - a restart.
  - `nosql-server/tests/drivers.rs::tls_on_both_ports`: the Rust `scylla` driver over rustls
    on both ports, with an untrusted certificate refused. The driver names servers by IP and
    sends no SNI, so it routes by user.
  - `nosql-server/tests/image.rs::tls_clients`: the image with `ROLL_NOSQL_TLS_CERT` /
    `ROLL_NOSQL_TLS_KEY`, PasswordAuthenticator and two shards. It checks:
    - roll's client;
    - `SCYLLA_SHARD_AWARE_PORT_SSL` (and no plain-text shard-aware port);
    - Python `cassandra-driver` 3.30.1 and `scylla-driver` 3.29.12 (one connection per
      shard over TLS);
    - Cassandra 5.0.9's `cqlsh --ssl` with `SSL_CERTFILE`;
    - the image's own `cqlsh --ssl --server-name`;
    - that plain text is refused.

    It was verified against `roll/nosql:dev` as built at 01:36 on 2026-10-21.
  - `nosql-server/tests/client_certs.rs`: client certificates (required, optional, untrusted,
    TLS and plain text on one port) and the certificate authenticators of both dialects, and
    `image.rs::tls_from_configuration_files`: TLS and client certificates from a mounted
    cassandra.yaml / scylla.yaml.
  - The PROXY protocol has unit-level tests in `node.rs`, and goes through rolld in the first
    test above.

## Per crate

### nosql-types

- **Covered:** every module has unit tests (comparison, validation, literals, JSON, type names,
  token, timeuuid). It is also exercised heavily through the engine, compat and conformance
  suites.
- **Gaps:** none worth a dedicated test.

### nosql-proto

- **Covered:**
  - codec tests: versions, oversized frames, v5 segment errors and a fuzz test;
  - `tests/loopback.rs`: every version and compression, auth, timeouts and closed
    connections;
  - `tests/reference.rs`: the shared references.
- **Filled:** client stream-id exhaustion (`loopback.rs::stream_id_exhaustion`). When all
  32,766 ids are held by timed-out requests, the next request fails at once with `Busy`, and
  the ids come back with the late answers.
- **Left:**
  - A server that never answers keeps the client `Busy` for good, where the Java driver would
    close the connection. This is a design choice; document it if it becomes a problem.
  - `encoded_len` and `raw_cell` are public but unused.

### nosql-cql

- **Covered:** round-trip corpora for both dialects, reference error texts, fuzzing and
  per-clause AST tests. `display.rs` has no unit tests of its own, but every corpus statement
  round-trips through it.
- **Gaps:** none.

### nosql-storage

- **Covered before:**
  - the LSM tree, SSTables, and cache tiers with corrupt disk blocks;
  - compaction strategies;
  - commit-log crash recovery, branches, restore and history;
  - GC references, snapshots, and the model property test.
- **Gaps found:**
  - the disk cache across a restart;
  - unload during a compaction;
  - failed uploads and manifest writes in flush, compaction, unload, truncate and drop (DDL
    rollback);
  - GC ageing over `min_age`, and log segments no manifest lists yet;
  - bulk ingest (only reached through nosql-control);
  - the `memtable_flush_period_ms` path of `maintain`.
- **Filled:**
  - All of the above are in `src/tests/lifecycle.rs`:
    - `disk_cache_starts_empty_after_a_restart_and_refetches_lost_files`
    - `unload_during_a_compaction_keeps_a_consistent_manifest` (5 timings)
    - `failed_sstable_uploads_keep_data_and_the_database`
    - `failed_manifest_writes_are_retried_and_ddl_survives_a_crash`
    - `garbage_collection_ages_candidates_and_keeps_unlisted_log_segments`
    - `bulk_ingest_swaps_atomically_and_survives_a_crash`
    - `maintenance_flushes_memtables_by_flush_period`
  - Failure injection is a `#[cfg(test)]` `failpoint` module in `lib.rs`, hooked into manifest
    writes and SSTable uploads. Patterns name the test's own database, so it works on S3 and
    in parallel.
- **Bug fixed:** `write_manifest` marked the manifest clean before storing it. After a failed
  PUT, the background writer never retried, so a TRUNCATE or DROP TABLE came back after a
  crash.
- **Bug fixed (with nosql-server):** REST snapshot listings lived only in memory; see
  nosql-server below.
- **Left:**
  - The `maintenance_loop` timers themselves. `maintain()` and `write_manifests()` are tested
    directly; the loop is a plain `interval`.
  - Unused public functions: `Database::table_ids`, `is_bottom`, `max_deletion`, `peek_u8`,
    `u32_le` and `source_open`.
  - The memory-budget work (`memory.rs`, `tests/memory.rs`) is in progress elsewhere and is
    not audited here.

### nosql-engine

- **Covered:**
  - about 300 unit tests by module (DDL, DML, select, LWT, auth, masking, UDF, derived data,
    DESCRIBE, system tables, guardrails, size limits, TTL, tracing, USING TIMEOUT);
  - the captured reference cases;
  - the compat scripts;
  - cqlpy in both dialects (see plan.md).
- **Gaps found:**
  - `per_partition_rate_limit` was parsed but never enforced, so RATE_LIMIT_ERROR was
    unreachable.
  - Read failures always reported consistency ONE.
  - Tombstone thresholds had no live test.
- **Filled:**
  - `rate_limit.rs` implements the limit following ScyllaDB's design: per-partition counters
    halved every second, and admission with probability `L/(x·ln2)`. Which statements count
    was measured on the reference:
    - counted: reads that name partitions, single writes, unlogged batches, and logged
      batches of one mutation;
    - not counted: scans, DISTINCT, LWT, counters, and logged batches of several mutations.

    It is tested in `nosql-server/tests/protocol.rs::per_partition_rate_limit`:
    CONFIG_ERROR without the extension, 0xF000 with the operation type and coordinator flag
    with it, and ALTER replacing the limits.
  - Read failures now carry the request's consistency, as Cassandra's do.
  - `tests::live::tombstone_failure_threshold` covers READ_FAILURE 0x1300 with the reason map
    `{node: 1}` (`tombstone_failure_threshold`, 100000 by default).
  - **Cassandra's tombstone warning** (`tombstone_warn_threshold`, 1000) is implemented in
    `statements/read_warn.rs`, a port of `ReadCommand`'s `MetricRecording` and of
    `ReadCommand.toCQLString` (column filter, partition key, token ranges, clustering slices and
    names, row filter, `ORDER BY`, `DataLimits`). It warns per read command as Cassandra splits
    a SELECT: one per partition of an `IN` (with the page's room as LIMIT), one per page of a
    scan (later pages start at `token(pk) >= token(<last key>)`), the internal pages of
    aggregates and GROUP BY. Storage records the counts after each row of the current chunk
    (`ReadTrace`), so a command stops exactly where Cassandra's limits stop it.
    - `tests::tombstone_warn::captured_tombstone_warnings` replays 84 query shapes on every
      page (`docs/nosql/reference/capture/tombstones.py`, generated by
      `scripts/gen_tombstones.py`) on memtables and after a flush: row, cell, range and static
      tombstones, LIMIT / PER PARTITION LIMIT / DISTINCT, ALLOW FILTERING (`=`, `IN [..]`,
      slices, `CONTAINS [KEY]`, map entries), DESC clustering, multi-column slices, token
      restrictions, a 600-character key (the 512-character cut), aggregates and GROUP BY
      (`GROUP LIMIT`, sub-pages of 10000 rows). ScyllaDB gives no warning for any of them.
    - `map_element_deletions` and `nosql-server/tests/compat/tombstones.cql` (1001 cell
      tombstones from one `DELETE`), with bound values.
    - Found and fixed on the way (Cassandra dialect): a `token(...)` restriction next to `pk =`
      no longer reads the partition, and a paged read whose last partition reached its
      `PER PARTITION LIMIT` / DISTINCT row ends without a paging state (no empty last page).
- **Left:**
  - **Tombstone warning, approximated:** an unpaged `IN` query that reaches its LIMIT before its last
    partition doesn't read the others (Cassandra reads them concurrently, so it warns for them,
    in completion order); GROUP BY with `PER PARTITION LIMIT` over a scan and later GROUP BY
    pages that cross partitions of an `IN` approximate the counts; a static-column filter
    that rejects a partition of a scan still counts its tombstones; reads through a secondary
    index don't warn.
  - **Multi-round SASL:** `AuthStep::Challenge` is unreachable. Both references'
    PasswordAuthenticator is single-round PLAIN, and roll's other authenticators (the
    certificate ones) take no SASL round either. What
    is reachable is tested: failed attempts leave the connection unbound and the retry may
    name another database (`node.rs::sasl_rounds_route_each_attempt`).

### nosql-alternator

- **Covered:**
  - unit tests;
  - `tests/e2e.rs`, `tests/http.rs`;
  - `tests/golden.rs`, a replay of reference transcripts;
  - the alternator conformance suite.
- **Golden replay hash order.** Of the 4 recorded differences, 3 are fixed. ScyllaDB lists
  vector indexes (`vec_index_create.txt:186/196/208`) in the iteration order of a libstdc++
  `unordered_map` keyed by name and filled in sorted order. That uses 13 buckets, growing to
  29, 59, 127 and so on, with libstdc++'s `_Hash_bytes`. `ops::vector::scylla_order`
  reproduces it, and it was checked against 7 reference probes and g++ itself. The entries
  are removed from both known-differences files.
- **Left:** one difference, `vec_system_tables.txt:15` (engine backend). The options map of
  the internal table prints as `dimensions, similarity_function, target, class_name` whatever
  the values. Several insertion orders explain that, while other maps print sorted. The rule
  can't be derived without ScyllaDB's source, which we may not read.

### nosql-lua

- **Covered:** one unit test plus `tests/coexist.rs`. Lua UDFs are tested end to end through
  the engine (`tests/udf.rs`, captured UDF cases) and cqlpy.
- **Left:** the raw FFI wrappers `lua_rawequal`, `lua_rawget`, `lua_rawgeti`, `lua_rawlen` and
  `lua_touserdata` are unused, so there is nothing to test.

### nosql-server

- **Covered:**
  - `protocol.rs`: handshakes, negotiation, decoding errors, oversized and corrupt frames,
    auth, events, tracing, the shard-aware listener and the clients tables;
  - `node.rs`: SASL and SNI routing, PROXY v1/v2 including with TLS, unload and reload,
    eviction, read-only databases, metrics, token routing;
  - `http.rs`, `rest.rs`, `alternator.rs`;
  - `drivers.rs`, `compat.rs`, `image.rs`, `tools.rs`, and conformance.
- **Gaps found:** REST snapshots across unloads; the rate-limit error path; multi-round SASL;
  stream-id exhaustion; TLS for real drivers and cqlsh; TLS in the image.
- **Bug fixed:** REST snapshots were kept only on the loaded database. After an unload,
  `listsnapshots` was empty, a tag could be reused, and clearing everything left the snapshot
  objects behind. Listings are now stored next to the snapshot
  (`Database::put_snapshot_info` / `snapshot_infos`) and deleted by `clear_snapshot`.
- **Filled:**
  - `rest.rs::snapshots_survive_unloads`
  - `node.rs::sasl_rounds_route_each_attempt`
  - `protocol.rs::per_partition_rate_limit`
  - `drivers.rs::tls_on_both_ports`
  - `image.rs::tls_clients`
- **Filled (feature):** client-certificate authentication. TLS listeners verify client
  certificates against a client CA (`net::ClientAuth`: `require_client_auth` true / optional),
  serve TLS and plain text on one port (`tls_optional`, Cassandra's `optional`), and pass the
  verified leaf (subject in RFC 4514 form, subject alternative names) to the engine's
  certificate authenticators (`nosql_engine::auth::certificate`): Cassandra's
  `MutualTlsAuthenticator` / `MutualTlsWithPasswordFallbackAuthenticator` (SPIFFE identity,
  `ADD IDENTITY`) and ScyllaDB's `CertificateAuthenticator` /
  `CertificateOrPasswordAuthenticator` (`auth_certificate_role_queries`, READY without SASL).
  `tests/client_certs.rs` covers every mode and error through raw frames, the nosql-proto
  client and the Rust `scylla` driver, on rcgen certificates.
- **Left:** the references run without TLS, so the AUTHENTICATE flow and texts are Cassandra's
  source (`MutualTlsAuthenticator`, `SpiffeCertificateValidator`, `ClientState.login`) and,
  for ScyllaDB, its documentation; ScyllaDB's error texts and the class name its
  `CertificateOrPasswordAuthenticator` sends without a certificate are not verified. Cassandra's
  identity cache (`credentials_validity`) isn't modelled.

### nosql-control

- **Covered:**
  - `api.rs`: every route's 404, create and patch validation, the console, passwords and
    deletion;
  - `branches.rs`;
  - `imports.rs`: `cql://` against the shared references;
  - `import_sstables.rs`: fixtures in every format, layouts, corrupt backups and live
    snapshots.
- **Gaps found:**
  - the expiry, import-schedule, metering and GC loops had no test at all;
  - `/alternator`'s 404;
  - `days` clamping and malformed query strings;
  - empty bodies, 405s and unknown routes;
  - every 500 path;
  - `s3://` imports.
- **Filled:**
  - `tests/loops.rs::loops_run_on_their_intervals`, on a paused tokio clock. Each loop does
    nothing before its interval and acts right after:
    - an expired branch is deleted and its parent kept;
    - a due import reruns and is rescheduled;
    - one metering sample per interval;
    - the first GC pass writes `nosql/gc/pending.json`.
  - `api.rs::query_strings_bodies_and_internal_errors`. A corrupt manifest gives 500
    `{"error"}` on `/schema`, `/compact` and `/cql`, and the database recovers once the
    manifest is repaired.
  - `import_sstables.rs::s3_backup` (`s3://` with `endpoint=`, an empty prefix, a missing
    bucket).
- **Bug fixed:** `GET /usage?days=<not a number>` answered with a plain-text 400 instead of
  `{"error": …}`.
- **Left:** `cql://` imports from live clusters run only with the shared references
  (`imports.rs`; they skip without them).

### nosql-sstable-import

- **Covered:**
  - every format (`nb`, `oa`, `da`, `me`, `ms`, `mt`; dictionary compression);
  - corrupt chunks, CRCs and statistics;
  - table discovery;
  - imports end to end through nosql-control.
- **Filled:** `fixtures.rs::damaged_and_unsupported_components`:
  - a `CompressionInfo.db` that `TOC.txt` lists but that is missing;
  - a non-numeric `Digest.crc32`;
  - a truncated `Scylla.db`;
  - a partitioner other than Murmur3;
  - a future version (`pa`);
  - pre-3.0 files in a backup.
- **Bug fixed (data loss):** pre-3.0 SSTables (`ka`/`la`) in a backup were silently skipped,
  so a partly un-upgraded backup imported with missing data. The import now fails with "pre-3.0
  sstables are not supported" (`layout.rs::is_sstable_file`).

### roll-cli (nosql)

- **Covered:**
  - every subcommand end to end (`nosql_api.rs`);
  - cqlsh against the references' golden output (`cqlsh.rs`, `cqlsh_live.rs`) and against
    roll (`cqlsh_roll.rs`, which needs `--features roll-server-tests`; `test-nosql.sh` and
    the CI s3 job now run it).
- **Filled:** `nosql_api.rs::files_paging_and_reports`, covering:
  - `exec -f` with `SOURCE`;
  - multi-page console output and `USE` carrying over;
  - `tablestats` filters and `--json`;
  - `status --json`;
  - `connect` without `--driver`.

  `cqlsh --ssl` with `SSL_CERTFILE` and `--server-name` is covered in `image.rs::tls_clients`.
- **Left:** "API unreachable" errors come from `main.rs` code shared by every product.

### rolld (nosql)

- **Covered:** `tests/nosql.rs`:
  - `nosql_only_serves_and_restarts`: single mode, every listener, Alternator, graceful stop
    and restart;
  - `tls_sni_and_proxy_protocol` (see TLS above);
  - `full_mode_with_postgres`: the Postgres control plane and proxy, Redis, streams and tasks
    next to nosql, with `/v1/health`, `/v1/projects` and `/v1/nosql` on one port, CQL, and
    writes kept across SIGTERM and restart. Queues are off to keep it small;
  - `bad_flags_fail_at_start`: an unknown write isolation, a malformed
    `--nosql-single-setting`, a bad `--nosql-from`, a missing certificate, and
    `--nosql-alternator-https` without a certificate.
- `--nosql-from` seeding, single-mode settings and `--nosql-metrics` are covered through the
  image (`image.rs`).
- **Left:** none known.

### deploy/nosql

- **Covered:** `image.rs` (configuration mapping, persistence, seeding, TLS), `tools.rs` and
  `drivers.rs::docker_drivers`.
- **Filled (feature):** `client_encryption_options` from a mounted cassandra.yaml (PEM key
  material as `PEMBasedSslContextFactory` reads it, files or inline; `optional`,
  `require_client_auth`, `native_transport_port_ssl`; a JKS keystore or an encrypted key stops
  the container with the conversion commands; an mTLS authenticator without client auth stops
  it with Cassandra's message) and scylla.yaml (`certificate`, `keyfile`, `truststore`,
  `require_client_auth`, the two `_ssl` ports, `auth_certificate_role_queries`).
  `image.rs::tls_from_configuration_files` runs both dialects.
