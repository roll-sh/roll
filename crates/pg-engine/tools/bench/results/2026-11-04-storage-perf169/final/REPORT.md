INCONCLUSIVE — storage preservation gate remains held.

The candidate has higher observed settled memory, an unresolved index outlier, and an unproven exact per-file close topology. Protocol completion and correctness passed; RUN-PASS does not mean performance acceptance. This is roll before/after only, with no PostgreSQL server performance claim.

Median [min–max], seconds, five timed runs per side. Profiled runs are excluded. Wall includes Docker exec/client start and command completion; server/client CPU comes from their separate matched cgroups.

| Workload | Baseline wall | Candidate wall | Baseline server CPU | Candidate server CPU |
|---|---:|---:|---:|---:|
| copy | 4.597 [4.239–4.799] | 4.284 [4.132–4.541] | 4.210 [3.999–4.477] | 4.119 [3.955–4.386] |
| pgbench | 3.135 [2.477–3.359] | 2.629 [2.572–3.608] | 3.611 [2.975–3.976] | 3.198 [3.097–4.175] |
| index | 1.025 [0.970–1.171] | 0.926 [0.867–2.556] | 1.453 [1.360–1.651] | 1.252 [1.145–2.538] |

All wall/CPU ranges overlap. The first candidate index sample was 2.556 s and 2.538 server CPU-s; the other candidate index samples were 0.867–0.930 s. Total index wall over five runs was 5.215 s baseline vs 6.197 s candidate; total server CPU 7.323 s vs 7.451 s. No outlier was dropped and no timed run was repeated.

Full pgbench initialization is end-to-end, including COPY, VACUUM and primary keys. pgbench-init-details.json separates driver wall, pgbench printed full total, and rounded subphase sums; subphase sums are not substituted for the full timer.

Settled anon+shmem, MiB, median of three 10 s windows after 15 s settling for each state:

| State | Baseline | Candidate | Change |
|---|---:|---:|---:|
| fresh | 12.9492 | 13.8398 | +0.8906 |
| loaded | 301.6953 | 309.4453 | +7.7500 |
| 100idle | 328.7734 | 336.5234 | +7.7500 |

Each state had identical sampled memory across its three windows. These windows share one process per side and do not provide independent process repetition. The observed increase is retained and is not waived. windows.jsonl retains CPU, memory and actual disk bytes per window; final-summary.json retains idle CPU medians/ranges. The 100 connections were explicitly counted idle in pg_stat_activity and exited with zero failed pgbench transactions.

Sampled peak anon+shmem and disk lifecycle medians, MiB:

| Workload | Peak memory B/C | Disk at command completion B/C | Settled disk B/C | Post-close disk B/C |
|---|---:|---:|---:|---:|
| copy | 132.02/131.34 | 285.62/37.63 | 35.05/37.63 | 32.08/32.07 |
| pgbench | 418.66/430.21 | 52.55/15.87 | 14.38/14.38 | 11.41/11.41 |
| index | 480.93/501.92 | 79.86/47.86 | 79.86/43.35 | 40.38/40.38 |

Disk traversal followed real bind directories and symlinks; no symlink-size proxy was used. Peaks are sampled lower bounds. Immediate write-footprint savings are not persistent post-close savings.

Close fixture (one case each; three 1M-row tables):

| Side | Clean close seconds | Sampled allocated high-water MiB | Allocated after close MiB |
|---|---:|---:|---:|
| baseline | 0.420436 | 99.082 | 96.113 |
| candidate | 2.895060 | 157.734 | 96.113 |

All three tables validated before and after reopen. Candidate before-close had five linked L1 files and one ordinary L1 file, but the ordinary file was not between linked ranges. No SST remained unchanged across the snapshot interval. This does not prove the requested exact per-file case, and close time cannot be attributed to that rewrite alone. Candidate sampled close CPU/IO lower bounds were 2.458 CPU-s and 98,414,592 bytes written; baseline counters vanished too quickly to retain nonzero final deltas. Zero is not a claim of zero work. The pre/post removed/replaced input SST totals 362,404,635 baseline / 103,427,857 candidate bytes include background changes between snapshots and are not exact close-rewrite bytes. Both final footprints were 100,782,080 allocated bytes.

Limiter evidence:

- COPY: about half sampled self CPU in session/copy.rs:1500 parse_copy_data, with InsertNode and SST encoding/compression work. Both profiles lost zero samples (195/204 samples). One connection thread parses the CSV; client CPU was well below one core.
- Full init: mixed parser, InsertNode, stored_tuple_len and SstWriter work (200/176 samples, zero lost). It includes index creation and maintenance; no single isolated storage phase accounts for its full time.
- Index: parallel build Plan::run, sorting, stored tuple/key encoding and unattributed libc/kernel work (92/81 samples, zero lost). This is coarse attribution and does not explain the C0 outlier.
- pressure/cgroup samples, competing process snapshots, CPU quotas/weights/affinity and per-device IO are retained. No timed cgroup CPU throttling occurred. The selected IO device is 9:2, avoiding RAID/member double counting. Profiles are separate runs and never enter timing medians.

Correctness and protocol audit:

- 30 timed runs, 6 separate profiles, 76 expected count/sum/index checks, zero correctness or workload command failures, zero workload timeouts/retries. CSV SHA17122cc009eb57b00500fe1e74cda9cfbe808ed874dbb8676e5ca60c7311049c; all logical inputs/settings matched. Payload checksum is a first-character integer sum plus id/bucket/length sums, not a cryptographic per-row SQL digest.
-One failed protocol attempt is preserved: v3 raw port probe rejected TIME_WAIT after successful B0; exit1 with all owned resources removed. A TCP control reproduced the false refusal, proved live-listener rejection, and tested v4. Continuation retained B0 and resumed C0.
-Negative controls: real no-service psql exit2, Docker exit1 and pipefail exit7 all stop the runner; existing names/data refused. FakeDocker proves v2 cleanup masking0 vs repaired failure1 and successful cleanup0. Final PASS is after cleanup+source stability only.
- 45 created container IDs independently confirmed absent, port53390 has no listener, no volumes created,40 datasets retained (about 1.3 GiB). No broad cleanup or other-agent resource changes.204 source/binary stability attestations pass.
-Candidate/base HEAD/index/tracked bytes and pre-existing untracked results preserved. Existing release artifacts were not rebuilt; new dist outputs only, in the two reserved targets (16GiB /2.1GiB).
-Build scheduling initially inherited host-wide affinity, corrected late baseline build before timing; reservation is proven for measured containers, not the earlier compile. Affects no timed sample.
-Analysis/exporter issues are retained in sealing-issues*.log and analysis-issues.log; canonical profile counts are profiles-summary-v2.json. Earlier summary.json/report-output.log are intermediate outputs.

Exact builds, both cargo build --locked --profile dist -p pg-engine --bin roll-engine:

- candidate source 79746f3174996b8047c4c0c30db87f345418b162; dist binary SHA256 377e85acc26bfae7074e081f9da5224cd6d24c5439526a82907bc20cf4480810; exit0, build wall900.286s.
- baseline source 5c31223568fead896be10e66edd59e3d5a261002; dist binary SHA256 146750fa01e1abb69311d32c69e3f9b4f690472ae7b1326cb5cc68bbaaf5ccb1; exit0, build wall991.393s.

Canonical files: REPORT.md, final-summary.json, audit-final.json, PLAN.md and amendments, runner-v4.py, resume-1.py, commands.jsonl, results.jsonl, windows.jsonl, all raw*.log, profiles-summary-v2.json, manifests/SST hashes, retained datasets. Final closed bundle is under sealed/final; its hash and owner-only remote commit are reported separately after sealing. No shared branch push, source edit, optimization or acceptance waiver.
