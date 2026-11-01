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
checksum computed independently of either server. Each operation checks its result
cardinality. The runner rejects errors, zero completed operations and invalid samples.
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
the databases. Verify the expected checksum for a different fixture size or date format.

```bash
python3 crates/pg-engine/tools/bench/paired_oltp.py --self-test
HEAVY_NICE=0 crates/pg-engine/tools/heavy.sh \
  python3 crates/pg-engine/tools/bench/paired_oltp.py /absolute/path/to/config.json
```

The supported cases are read-only. Durable write comparisons require separate fixtures
and checks against each run's successful mutation count.
