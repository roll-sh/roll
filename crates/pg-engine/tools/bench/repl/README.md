# Run logical replication checks

These scripts use PostgreSQL 18.6 and an engine binary supplied through `ENGINE`.
Run them from the repository root through `tools/heavy.sh`.

Choose a fresh `A` prefix, your assigned twenty-port range, and a disk-backed `WORK`
directory. Set `PG_CPUS`, `E_CPUS`, and `C_CPUS` to available CPU sets if the defaults
20–23, 24–27, and 28–29 do not fit the host.

```sh
export A=a152-check PORT0=53040
export ENGINE="$PWD/target/debug/roll-engine"
export WORK="$HOME/.cache/$A"
crates/pg-engine/tools/heavy.sh timeout 7200 \
  sh crates/pg-engine/tools/bench/repl/e2e.sh
```

`e2e.sh` compares PostgreSQL and engine publishers with PostgreSQL and engine
subscribers. It checks row digests, subscription state, and conflict statistics.
Pass scenario names to run a subset, for example `stream_abort via_root twophase`.
An unexpected query failure, timeout, or differing result makes the script fail.

The other scripts measure specific workloads:

- `apply.sh` applies a transaction backlog to each subscriber separately. `PUB`
  chooses `pg` or `engine`. `BASE` adds another engine subscriber.
- `pubn.sh` sends a transaction backlog to the subscriber counts in `NS`.
- `pgb.sh` alternates `BASE` and `ENGINE` through select-only and TPC-B-like pgbench
  workloads. Both binaries are required.

For tooling smoke checks, use `ROUNDS=1 TXNS=40` with `apply.sh`,
`ROUNDS=1 TXNS=40 NS=1` with `pubn.sh`, and `ROUNDS=1 DUR=1 SCALE=1` with `pgb.sh`.
Use a fresh prefix and work directory for each invocation. These small debug runs
check the scripts and data, but do not establish comparative performance.

The helpers refuse occupied ports and existing database containers from another
invocation. They label containers with the worktree, limit each to 2 GB, stop the
resources they start, and retain containers and data directories for inspection.
Repeated rounds archive their stopped PostgreSQL containers and engine data.
The scripts require Linux, Docker, `taskset`, `ss`, and cgroup v2 CPU accounting.
Use release binaries and repeated interleaved runs before reporting performance.
