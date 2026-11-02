# Daemon lifecycle validation, 2026-11-02

The actual `rolld` executable passed its request lifecycle test against official
PostgreSQL 18. This is service verification, with no production runtime changes,
custom-engine integration, performance measurement, or distribution acceptance.
The source baseline is `b119df4d`; [result.json](result.json) records changed-file
hashes and the daemon binary hash.

The [service test](../../service.rs) checks API project creation, proxy requests,
rollback, four overlapping write transactions, CPU limits changing from
250,000,000 to 500,000,000 NanoCPUs with the backend session preserved, data after
suspend/resume, and adoption of a surviving compute after daemon process loss.
It also checks a new write and the history API's reported WAL acknowledgement
after restart. That acknowledgement does not independently prove spool contents
or object-store durability. [daemon.txt](daemon.txt) retains the daemon log,
including a base-backup cancellation during suspension.

| Check | Result | Record |
| --- | --- | --- |
| Workspace `cargo build` | Passed | [build.txt](build.txt) |
| Workspace `cargo test` | Interrupted, exit -15; not passed | [test-interrupted.txt](test-interrupted.txt) |
| Workspace `cargo clippy` | Passed with 11 existing engine warnings | [clippy.txt](clippy.txt) |
| Service build and all-target Clippy with warnings denied | Passed | [postgres.txt](postgres.txt) |
| Service unit tests | 24 passed | [postgres.txt](postgres.txt) |
| Actual-daemon lifecycle | 1 passed | [postgres.txt](postgres.txt) |
| Control-plane Docker suite | 12 passed, 4 failed; exit 101 | [postgres.txt](postgres.txt) |

The full service command was:

```sh
ROLL_E2E_BIND=0.0.0.0 ROLL_E2E_THREADS=2 pnpm test:pg --force
```

[gates.json](gates.json) records commands, timestamps, and exits. The broad
workspace test run was interrupted while checking unchanged engine goldens;
the service gate completed. No fresh full-workspace test pass is claimed.

Point-in-time branching, restore-in-place, history retention, and local-disk-loss
recovery failed at the bounded WAL-route preflight. Requests from inside the
compute timed out instead of receiving HTTP 403 for an invalid token. Correct-token
fetching, byte equality, and subsequent recovery remain unverified. A separate
[network diagnostic](network-preflight.json) also timed out through both tested
Docker host gateway addresses. Firewall rules and production networking were
unchanged. The replica test passed after a long wait; that does not establish
reliable recovery transport.

[cleanup.json](cleanup.json) records absent owned processes and containers,
closed service ports, and removal of successful service state. The daemon log
was retained. Those checks describe the captured cleanup state.

Verify the exported file hashes from this directory with `sha256sum -c SHA256SUMS`.
Hash verification checks bytes, not gate acceptance. TLS, arbitrary unchanged
applications, operating cost, lower CPU, multi-host placement, fencing, failover,
and distributed SQL remain open in the
[product requirements](../../../../pg-engine/ACCEPTANCE.md).
