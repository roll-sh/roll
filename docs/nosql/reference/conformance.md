# Reference captures and conformance baselines

This directory is the ground truth for roll nosql: what `scylladb/scylla:2026.3.3` and
`cassandra:5.0.9` do, observed by us, plus the conformance runner's reference baselines.

| file | contents |
|---|---|
| [supported.md](supported.md) | protocol versions, `SUPPORTED`, handshake and frame-level errors (raw sockets) |
| [system-local-peers.md](system-local-peers.md) | `system.local`, `system.peers(_v2)` and ScyllaDB's node tables: columns, types, values |
| [system-schema.md](system-schema.md) | every `system_schema` table, and the rows a sample schema produces in each dialect |
| [describe.md](describe.md), [describe-system.md](describe-system.md) | server-side `DESCRIBE` result rows, verbatim |
| [default-options.md](default-options.md) | default table and keyspace options, client-relevant server settings |
| [system-tables.md](system-tables.md) | every keyspace and table on a fresh node, virtual tables, columns |
| [errors.md](errors.md) | about 750 error, warning and auth cases side by side, with codes and exact messages |
| [statements.md](statements.md) | result metadata, rows, paging, LWT results and prepared metadata per statement family |
| `capture/` | the scripts that produced all of the above (our own code) |

## How the captures were made

- **References.** The captures ran against long-lived containers on one machine:
  - ScyllaDB: `--smp 1 --memory 1G --overprovisioned 1 --developer-mode 1 --alternator-port 8000
    --alternator-write-isolation always_use_lwt --api-address 0.0.0.0`.
  - Cassandra: the image defaults, with `MAX_HEAP_SIZE=640M HEAP_NEWSIZE=128M` (smaller than
    compatibility.md's 1G because the machine was short of memory; only heap-derived settings in
    `system_views.settings` are affected).
  - The auth cases ran on two more references: the same images with only `PasswordAuthenticator`
    and `CassandraAuthorizer` added (ScyllaDB with `--memory 750M`).
  - The "features enabled" parts of system-schema.md and describe.md ran on a node started with
    the suites' flags (see below).
- **Clients.** The scripts run in the runner image (`scripts/nosql-conformance/Dockerfile`, Python
  3.12, `scylla-driver` 3.29.7) inside the reference container's network namespace.
  - `capture/raw.py` is a minimal native-protocol client. Every error, warning and schema-change
    body in errors.md and supported.md comes from the bytes on the wire, not from the driver.
  - Metadata and rows come from the Python driver.
- **Running them.**
  - `capture/cap.sh <dialect> <script> <args>` runs a script against `nosql-ref-<dialect>`.
  - `capture/auth_ref.sh` starts a dedicated node with the suites' flags and runs the
    feature-enabled catalog.
  - `python3 capture/render.py docs/nosql/reference` regenerates the Markdown from `capture/out/`.
    That directory is not committed.
- **Volatile values** (host ids, addresses, tokens, schema versions, table ids, gossip
  generations, salted hashes) are masked where they appear, and each file says which values are
  volatile.
- **Destructive statements.** `TRUNCATE system.local`, `INSERT INTO system.local …`,
  `DELETE FROM system_schema.keyspaces …` and `ALTER KEYSPACE system_auth …` are accepted by both
  references, and they break the node: drivers cannot connect, and ScyllaDB's Raft group 0 stops
  with `std::out_of_range (map::at)`. They were observed once and recorded in errors.md under
  "Writes to system keyspaces". They are no longer probed. Never run them against a shared
  reference.

## Conformance runner

`scripts/nosql-conformance.sh` runs ScyllaDB's `test/cqlpy` and `test/alternator` suites against
a target. The suites are under ScyllaDB's source-available licence, so the repository stores only
test ids.

- **Pinned suites.** Commit `d80b3d6ca26b5457c1b59b47046fab27fa1953f3`, the tag `scylla-2026.3.3`.
  This is the build inside the scylla reference image (`2026.3.3-0.20261004.d80b3d6ca26b`).
  - The script fetches it shallow, blobless and sparse into
    `target/nosql-conformance/scylladb/<commit>/`. The checkout has `test/{cqlpy,alternator,pylib}`,
    the `test/*.py` files, `test/pytest.ini` and `test/cluster/util.py`, which
    `cqlpy/test_keyspace.py` imports.
  - It mounts the checkout read-only into the runner container.
- **Runner image.** `scripts/nosql-conformance/Dockerfile`, tagged by the hash of that directory.
  - It is based on `python:3.12-slim` with pinned `pytest` 8.4.2 (9.x breaks the suites'
    conftest), `pytest-asyncio`, `scylla-driver` 3.29.7, `boto3`, `requests`, `allure-pytest`,
    `SQLAlchemy` (the suites' `filterwarnings` imports it) and the other modules the suites import.
  - It also has the docker CLI.
  - `results.py` is a pytest plugin that writes the sorted `test_id PASS|FAIL|SKIP|XFAIL|ERROR` list.
- **Targets.**
  - `--reference scylla|cassandra` starts `nosql-refs-<dialect>` with `--rm` and a memory cap
    (ScyllaDB 3g, because Seastar subtracts its own reserve from the cgroup limit; Cassandra 2560m
    with a 640M heap). It stops the container on exit through a trap and keeps its log in
    `target/nosql-conformance/out/reference.log`.
  - `--roll scylla|cassandra` builds and starts `crates/nosql-server/examples/conformance_server.rs`
    on 127.0.0.2:9042 (CQL) and 127.0.0.2:10000 (REST).
  - `--target-container NAME` joins a running container's network namespace.
  - `--host/--port` (and `--alternator-url`) use host networking.
- **`--reference-config suite`** is the default for `--reference` and `--roll`. It uses the flags
  of the suites' own `test/cqlpy/run`, `run-cassandra` and `test/alternator/run`:
  - ScyllaDB:
    - PasswordAuthenticator and CassandraAuthorizer with superuser `cassandra`/`cassandra`
    - UDFs (`--experimental-features=udf`), tablets, `--strict-allow-filtering=true`
    - long server timeouts and the Alternator flags (`--alternator-write-isolation
      only_rmw_uses_lwt`, `--alternator-enforce-authorization 1`, …)
  - Cassandra: the same authenticator and authorizer, with UDFs, MVs and SASI enabled.
  - `--reference-config default` runs the plain image instead.
- **REST API and nodetool.**
  - The suites reach ScyllaDB's REST API at `<host>:10000`, which is hard-wired. The reference
    therefore runs with `--api-address 0.0.0.0`, and roll binds its REST API next to its CQL port.
    When there is no REST API, tests that need it skip themselves.
  - Against Cassandra the suites call `$NODETOOL -h <host> …`. Cassandra's JMX only listens inside
    its container, so `NODETOOL=nodetool-docker`, an image script that drops `-h` and runs
    `docker exec <container> nodetool` through the mounted docker socket.
- **Disruptive tests.** On a full run, these tests run last, each in its own pytest process:
  - `cqlpy/test_native_transport.py::test_enable_disable_binary` turns CQL off and on through the
    REST API.
  - `cqlpy/test_secondary_index.py::test_create_index_oversized_name` makes Cassandra stop its
    transport after a file-system error on the 500-character index name.

  The suites' conftest declares the server crashed when one test leaves it unreachable, then
  skips every later test. Before this change, one such test turned about 400 results into SKIP or
  ERROR.
- **Other details.**
  - Paths go before options: pytest only registers the suites' `--host`/`--port` options for
    paths it has already seen.
  - The reference is addressed by IP, because `test_localnodes` compares the endpoint with the
    node's address.
  - Alternator runs pass `--runveryslow` (10 tests). Nightly-marked tests are included, and every
    test gets a 900 s timeout.
- **Machine limits.**
  - ScyllaDB needs AIO capacity (`fs.aio-max-nr`), so the script passes
    `--max-networking-io-control-blocks=1000`.
  - On a host already running several ScyllaDB processes, a new one can still fail with "Your
    system does not satisfy minimum AIO requirements". Raise `fs.aio-max-nr` or stop the other
    instances.
  - `ROLL_CONFORMANCE_MIN_MEM_GB` waits for free memory before each container start.
  - `ROLL_CONFORMANCE_MAX_REFS` (default 2) waits while that many reference containers already run.

## Reference baselines

`crates/nosql-server/tests/conformance/reference-*.txt` list `test_id RESULT`, sorted (C
locale). They record what each reference does with `--reference <dialect>` (suite config). A test
that `PASS`es there must pass on roll in the same dialect (`--compare`).

cqlpy was run twice per reference. A test whose result differed between runs is recorded as
`FAIL` (it is flaky on the reference itself):

- ScyllaDB: `test_tablets.py::test_tablets_are_dropped_when_dropping_index[False]` and
  `::test_tablets_are_dropped_when_dropping_table_with_view`.
- Cassandra: `insert_update_if_condition_statics_test.py::testConditionalUpdatesOnStaticColumns[vnodes]`
  and `test_lwt.py::test_lwt_static_condition`.
- Alternator: `test_tag.py::test_concurrent_tag` failed in 2 of 3 runs and is `FAIL`.

| suite | reference | tests | PASS | FAIL | SKIP | XFAIL | ERROR | duration per run |
|---|---|---|---|---|---|---|---|---|
| cqlpy | ScyllaDB 2026.3.3 | 2532 | 2009 | 28 | 272 | 223 | 0 | 301 s and 361 s |
| cqlpy | Cassandra 5.0.9 | 2532 | 1214 | 233 | 983 | 102 | 0 | 1269 s and 1301 s |
| alternator | ScyllaDB 2026.3.3 | 1426 | 1253 | 3 | 50 | 120 | 0 | 146 s |

Durations are the script's wall-clock time for the pytest runs on this machine (shared
with other jobs), excluding the reference's startup.

Why tests fail or skip on the references themselves:

- **cqlpy on ScyllaDB.**
  - Most failures need a local ScyllaDB process: its data directory, `scylla sstable`, WASM
    examples built from source (`test_wasm.py`), or the client address `127.0.0.1`
    (`test_virtual_tables.py::test_clients`).
  - Others are timing-sensitive or expect messages from a newer build: `test_using_timeout.py`,
    `test_using_service_level.py`, `test_shedding.py`, `test_guardrail_replication_strategy.py`
    and `test_describe.py::test_hide_*`.
  - Skips are mostly "Can't find local Scylla process" (177), Vector Store mocks (32) and issue
    markers.
- **cqlpy on Cassandra.**
  - 698 tests are skipped as ScyllaDB-only.
  - Most failures are `XPASS(strict)`: the test is marked xfail for a ScyllaDB bug and Cassandra
    passes it.
  - The rest are ScyllaDB-specific syntax and options, such as `tablets`, `HASHED PASSWORD` in
    DESCRIBE and `SELECT … FROM system.config`.
- **alternator.**
  - The remaining failures are `test_encoding.py::test_table_naming` (it sees other tables on the
    node), `test_scan.py::test_scan_paging_missing_limit[1500]` (an `XPASS(strict)`) and the flaky
    tag test above.
  - Skips: transactions (44), the Vector Store (`--vs`, 37), HTTPS-only tests and issue markers.

## Invoking

```sh
scripts/nosql-conformance.sh --reference scylla                    # cqlpy on the ScyllaDB reference
scripts/nosql-conformance.sh --reference cassandra                 # cqlpy on Cassandra
scripts/nosql-conformance.sh --reference scylla --suite alternator
scripts/nosql-conformance.sh --roll scylla --compare crates/nosql-server/tests/conformance/reference-scylla-cqlpy.txt
scripts/nosql-conformance.sh --roll cassandra --filter json -- cqlpy/test_json.py
scripts/nosql-conformance.sh --host 127.0.0.1 --port 9042 --junit out.xml
```

Results go to `target/nosql-conformance/<suite>-<target>.txt`, or the path given with
`--results`. The pytest log goes to `target/nosql-conformance/out/pytest.log`. The exit status is
non-zero when pytest itself errors or `--compare` finds a test that passes in the baseline but not
here and is not in the known-failures list. With `--filter` or explicit paths, only the tests that
ran are compared.

- **Known failures.** `--known-failures FILE` (default with `--roll <dialect>`:
  `crates/nosql-server/tests/conformance/roll-known-failures-<dialect>.txt`; `none` disables it)
  lists `test_id  # tag: note` lines. Listed tests may fail; listed tests that pass are printed
  so they can be removed, because the list may only shrink.
- **roll health.** With `--roll`, the run also fails when roll exits early or logs a panic
  (`target/nosql-conformance/out/roll.log`).
- **Build.** `--roll` builds with `$CARGO` (default `cargo`; a wrapper script works) and finds
  the binary from cargo's JSON output. `ROLL_CONFORMANCE_TIMEOUT` (default 900) is the per-test
  timeout in seconds.
