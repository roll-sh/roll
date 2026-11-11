# Checked OLTP measurements, 2026-11-02

The histogram change at `5b32046d` improves eight-client ordered range throughput
from 8,419 to 8,829 operations/s against release baseline `7f35a185`. Server CPU
falls from 469 to 446 microseconds per operation. Each figure is the median of
five alternating pairs after a warmup pair. PostgreSQL remains about 4.3 times
faster in the final range comparison. No memory advantage is established.

[BENCH.md](../../../../BENCH.md) explains the controls, settings, and limits.
`comparisons.json` preserves configurations, all summary statistics, and available
runtime observations. `oltp.tsv` contains warmup round zero and measured rounds
one through five. `raw/` contains the original invocation logs, resource records,
and fixture digests. Source paths and PIDs in these historical records refer to
the original machine and are not instructions to rerun destructive setup.

Run the read-only audit from any working directory:

```bash
python3 /path/to/results/2026-11-02/verify.py
```

The audit checks file hashes, every result's validation contract, operation counts,
resource counters, fixture digests, recomputed summaries, gate statuses, and both
regression reports. `SHA256SUMS` detects accidental artifact changes; it is not a
signed attestation. The original full-source audit, `audit-final.py`, also checked
frozen executable hashes and measured-source identity on the original machine.
It requires that machine's cache and checkout.

`final-verification.json` records 430 passing crate tests, 18 ignored tests, build,
Clippy, PostgreSQL units, and 51,148/51,150 matching regression units. Both existing
regression differences are byte-identical to baseline. Clippy includes one new
benchmark style warning alongside the existing library and test warnings.
Docker recovery e2e retains the existing waiver and baseline of twelve passes and
four WAL HTTP timeouts. `final-cleanup.json` records stopped campaign services.

The initial baseline comparison used checked client `55fe2f7b`. Later comparisons
used `89e863fc`, which additionally rejects malformed NUMERIC encodings that would
round to an expected integer. Both report `oltp-values-v1`; the distinct executable
hashes and manifests identify their different validation strength. Secondary
count/sum checks cannot prove the identities of all contributing rows.

Historical build records do not establish reproducible builds or complete clean
source provenance. The initial PostgreSQL comparison lacks an exact image identity
and observed CPU-budget snapshot. Plan-choice and negative-control records lack
complete execution identities. Later immutable runtime observations describe their
own runs and cannot establish those earlier facts. `runtime-latest.json` describes
the last restart, not every comparison.

Raw binaries, profiles, saved fixture data, and complete commands remain in
`~/.cache/pg-engine-perf-20261101/oltp-20261102/` on the original machine.
