# Checked comparisons

Build both server revisions in release mode with the same Cargo targets and feature set.
Copy each server binary aside before rebuilding, and record its source commit, command
and SHA-256. Use the same checked client binary for both endpoints.

Use dedicated databases with identical deterministic fixtures. Verify counts and
checksums independently before timing. For each endpoint, record actual settings,
durability, locale, plans, CPU allocation and resource limits. Pass the fixture's row
count to both the loader and query runner. Stop an engine cleanly before copying its
data directory.

`paired.py` checks analytic wire queries with one warmup pair and at least five alternating
pairs. It saves every command and complete output, checks every returned row count and
hash across all samples, and reports medians and ranges. Queries receive and hash the
complete response inside the timer. Output directories must be new.

Example configuration for two existing engine servers:

```json
{
  "bench": "/absolute/path/to/frozen/bench",
  "out": "/absolute/path/to/new-results",
  "db": "dedicated_perf_200000",
  "rounds": 5,
  "client_cpus": "32-35",
  "expected_rows": {"top_n": 100},
  "args": ["--rows", "200000"],
  "targets": [
    {"name": "before", "port": 53101, "probe": {"pid": 12345}},
    {"name": "after", "port": 53102, "probe": {"pid": 12346}}
  ]
}
```

Replace the paths, PIDs, ports and CPU allocation with the running fixtures' values.
For PostgreSQL, use `"probe": {"cgroup": "/absolute/cgroup/v2/directory"}`. The directory
must contain `cpu.stat` and `memory.stat`. Process memory is RSS; cgroup memory is
`anon + shmem`. These are different measures and exclude filesystem cache differently.
Process CPU currently assumes 100 clock ticks per second; verify `getconf CLK_TCK` before
using that metric. CPU samples near the accounting resolution cannot establish a gain.

From the repository root:

```bash
python3 crates/pg-engine/tools/bench/paired.py --self-test
python3 crates/pg-engine/tools/bench/check_suite.py
HEAVY_NICE=0 crates/pg-engine/tools/heavy.sh \
  python3 crates/pg-engine/tools/bench/paired.py /absolute/path/to/config.json
```

The runner requires exact result hashes. Floating-point aggregate differences require
inspection of the actual values and an explicit tolerance; they are not automatically
waived. Profile in a separate diagnostic run. Check client CPU as well as server CPU
before attributing a throughput ceiling to the server.

`suite.sh` runs broader workloads against one target. It rejects command failures,
missing result records, reported errors and invalid pgbench summaries, and reloads each
analytic fixture even in-process. It does not compare results across servers or replace
an alternating comparison. In-process runs require the wire engine to be stopped.

## Read-only OLTP comparisons

`paired_oltp.py` alternates checked read workloads, with one warmup pair and five recorded
pairs. It verifies the complete accounts fixture before and after the run, using an expected
checksum computed independently of either server. Every timed operation validates its
response values and cardinality before it counts as successful. The runner requires
`validation_version=oltp-values-v1` and `checked_ops=ops`; missing or unsupported versions,
missing or mismatched counts, errors, zero completed operations and invalid samples fail.
It records the benchmark binary hash, engine executable hashes, exact operation counts,
latency quantiles, client CPU, server CPU and memory samples.

Server CPU includes connection and statement setup for each complete client invocation;
the latency and throughput timer starts after that setup. CPU per operation divides that
complete invocation's CPU delta by its exact successful operation count. Process CPU uses
the host's `getconf CLK_TCK`; PostgreSQL cgroup CPU uses `cpu.stat`. Memory has the same
RSS versus cgroup limitations described above.

For the deterministic fixture produced by `oltp-load --rows 200000`, an example is:

```json
{
  "bench": "/absolute/path/to/frozen/bench",
  "out": "/absolute/path/to/new-oltp-results",
  "db": "dedicated_perf_oltp_200000",
  "rows": 200000,
  "rounds": 5,
  "seconds": 2,
  "cases": {
    "point_select": [1, 8],
    "point_select_simple": [1, 8],
    "range_scan_100": [1, 8],
    "secondary_lookup": [1, 8]
  },
  "psql_container": "owned-postgres-client-with-host-networking",
  "expected_fixture": "200000|20000100000|4999900000|9999900000|ac8ccd2a44331a8aa083be3b8e06ee5f",
  "targets": [
    {"name": "before", "port": 53101, "probe": {"pid": 12345}},
    {"name": "after", "port": 53103, "probe": {"pid": 12346}}
  ]
}
```

Replace the paths, database, container and probes with the owned fixtures' values. The
client container must reach both loopback ports and have `psql`; the runner only reads
the databases. Fixture checksum connections use `PGDATESTYLE=ISO,YMD`. Recompute the
expected checksum independently for a different fixture size.

```bash
python3 crates/pg-engine/tools/bench/paired_oltp.py --self-test
HEAVY_NICE=0 crates/pg-engine/tools/heavy.sh \
  python3 crates/pg-engine/tools/bench/paired_oltp.py /absolute/path/to/config.json
```

The `oltp-values-v1` contract is:

- `point_select` and `point_select_simple`: exactly one row for the sampled `g`, with
  `(id, owner, balance, note, created)` equal to `(g, g % 50000, (g * 7919) % 100000,
  'note ' || g, date '2024-01-01' + (g % 365))`. All five fields must be non-NULL.
- `range_scan_100`: exactly 100 rows in the query's explicit `ORDER BY id`, with row `j`
  equal to `(s + j, ((s + j) * 7919) % 100000)` for `j = 0..99`. Both fields must be
  non-NULL. The original sampled bound remains `1..N-100`.
- `secondary_lookup`: exactly one row, containing a non-NULL INT8 count and a NUMERIC sum.
  For owner zero the first ID is 50000; otherwise it is the owner. With `first` as that
  ID, `n = 0` when `N < first`, otherwise `n = 1 + (N - first) / 50000`. Set
  `a = (first * 7919) % 100000`, `b = (a + 50000) % 100000`. The exact sum is
  `ceil(n/2) * a + floor(n/2) * b`, or SQL NULL when `n = 0`. There is no float conversion
  or tolerance; zero is distinct from NULL.
- `select_1`: exactly one non-NULL INT4 value equal to 1.

Prepared wire reads check exact column OIDs and decode binary INT4, INT8, TEXT, DATE and
NUMERIC values. Small DATE/NUMERIC `FromSql` wrappers use the existing exact numeric
decoder; no dependency was added. In-process reads check the reported OIDs and native
value variants. Simple wire reads retain NULLs and check canonical text values, including
ISO dates after `SET DateStyle = 'ISO, YMD'` during setup. **tokio-postgres discards
simple-query type OIDs**, so that path cannot verify the server's declared types.
Expected text dates are cached for the fixture's 365 days using chrono, independently
of engine date formatting; integer/note comparisons avoid allocating expected strings.

Each bound is sampled once and reused for the query and its expectation. The fixture,
query SQL, random stream, binary INT4 wire parameters and text OID-23 in-process
parameters remain unchanged. `--rows` must be a positive INT4 for loading and queries;
selecting the range case additionally requires `N >= 101`. The runner enforces these
bounds before creating output or contacting either endpoint.

The supported paired cases are read-only. The existing write workloads and their
cardinality checks are unchanged. Run checked account reads against an unmodified
fixture: a mixed invocation with multiple client counts can read after earlier writes
and correctly fail the deterministic checks. Use `--only` to separate read and write
runs, and reload before reads. Durable write comparisons require separate fixtures and
checks against each run's successful mutation count.

Cheap validator controls (no database required):

```bash
cargo test -p pg-engine --example bench oltp_validation_tests
python3 crates/pg-engine/tools/bench/paired_oltp.py --self-test
```

The Rust controls cover wrong fields, NULLs, types, cardinalities, duplicate/reordered
ranges, stale parameter responses, owner and odd/even aggregate boundaries, fractional
numeric values, date decoding and leap-day text. Python controls reject unsupported or
missing validation, missing/mismatched checked counts, duplicate fields and invalid
fixture sizes, in addition to the existing sample checks. These are validator tests;
they do not replace live protocol checks against both endpoints.

Freeze the new checked client once and use that identical binary on **both endpoints**
for every new measurement. Old cardinality-only results are not validated retroactively.
Validation is inside operation latency/throughput timing. Compare the recorded client
CPU per operation as well as server CPU, and check whether the client limits throughput;
no performance claim follows from the validator change alone.
