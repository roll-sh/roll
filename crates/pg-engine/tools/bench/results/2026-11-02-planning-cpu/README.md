# CPU ancestry in prepared-range execution

Both custom binding and the later root-cost calculation are substantial investigation
targets in this frozen engine. Three twenty-second auto/eight-client profiles recover
inclusive `plan_cost`/`plan_cost_estimate` ancestry in 19.19–20.55% of recorded server
CPU-clock sample weight, and `bound_custom` ancestry in 17.24–17.70%.
These are recovered call-chain observations, not estimator self time or a speedup.

The workload is the unchanged named prepared statement:

```sql
SELECT id, balance FROM accounts WHERE id BETWEEN $1 AND $1 + 99 ORDER BY id
```

The fixture has 200,000 rows. The frozen histogram engine, checked client, binary
INT4 bindings and binary results match the earlier [planning-mode diagnostic](../2026-11-02-planning/README.md).
Every operation validates all 100 ordered ID/balance pairs. Three independent clones
start from the same stopped fixture. Server threads use CPUs 28–31; clients use 32–35.
`pg_stat_statements` is disabled for these CPU captures.

## Repeated profiles

Three alternating forced-custom/forced-generic pairs use one client. Three auto-mode
captures use eight clients. Each twenty-second recording starts two seconds into a
thirty-second client invocation, at 49 Hz with a 65,528-byte DWARF stack snapshot.
There is also one ten-second unprofiled invocation per condition to expose gross
profiler overhead. Those single controls cannot establish a precise overhead estimate.
Profiled throughput is diagnostic and is not reported as a performance gain.

| Condition | Captures | Samples | Recovered cost-wrapper/estimator ancestry, % | Recovered custom-binding ancestry, % |
| --- | --- | --- | --- | --- |
| Auto, 8 clients | 3 | 11,512 | 19.19 / 20.55 / 19.57 | 17.60 / 17.24 / 17.70 |
| Forced custom, 1 client | 3 | 2,420 | 19.75 / 18.46 / 19.80 | 17.14 / 19.21 / 20.42 |
| Forced generic, 1 client | 3 | 2,225 | 0 / 0 / 0 recovered | 0 / 0 / 0 recovered |

The nine captures contain 16,157 samples with zero original LOST or throttle records.
All recorded periods equal 20,408,163 ns of CPU-clock weight. The twelve complete client invocations return
1,360,591 checked operations with zero errors. Sampling windows exclude parts of those
invocations, so their operation totals cannot yield phase microseconds per operation.

Buckets are disjoint and include descendants. The cost bucket combines
`Session::plan_cost` with `plan_cost_estimate`. Wrapper-only ancestry contributes
0.29–0.73 percentage points in auto and 0.37–0.62 points in forced custom.
This does not establish that the estimator itself consumes 20% of CPU.

Finite stack snapshots can omit ancestors, including in otherwise resolved buckets.
Recovered ancestry is a lower bound within these samples, not a statistical lower
confidence bound on a population CPU share. Remaining sample weight stays in its
recorded categories and is never redistributed to planning. No recovered generic
ancestry through these boundaries does not establish zero generic planning cost.
Three captures support the diagnostic observation, not a production-wide CPU share,
removable-cost prediction, or optimization acceptance. Generic/custom bounds and
parameter folding also differ. Keep these sampled CPU weights separate from the
previous diagnostic's overlapping, instrumented elapsed timers.

## Correcting the decoder's address mapping

The initial pilot captured 79 samples with zero loss, but most original call chains
stopped at one to three physical frames. A larger stack snapshot did not fix this.
Independent inspection reproduced a one-page relocation error in perf 6.15's libdw
unwind path using the saved registers, stack bytes, and original ELF CFI.

The executable mapping's file offset and ELF virtual address differ by `0x1000`.
Perf's `map_start - map_pgoff` bias is therefore one page too high for CFI lookup.
For example, pilot IP `0x5559dac4fd32` has original load bias `0x5559d79ff000`,
so its ELF address is `0x3250d32`. The faulty decoder looks up `0x324fd32`.
Those locations use different CFA offsets, producing bogus callers from ordinary
stack data. The original snapshots contain the genuine return addresses.

[normalize-maps.py](normalize-maps.py) creates a synthetic replay copy. It derives
mapping offsets from the original ELF's PT_LOAD headers and changes only three
MMAP2 `pgoff` fields. The adjustments are `+0x1000` for executable text, `+0x2000`
for RELRO/data, and `+0x3000` for writable data. The first mapping, containing
`.rodata` and `.eh_frame`, stays unchanged. Original capture bytes are preserved.
Every replay record contains the original hash, replay hash and exact patch offsets.

Perf's symbol labels on those synthetic mappings are shifted and are unsuitable
for attribution. The decoder emits raw IPs only. [classify.py](classify.py) resolves
them using the original mapping bias and symbol ranges from the frozen original ELF.
It checks sample count, IP, timestamp, PID/TID, loss records, and bucket exclusivity.
Independent review additionally matched every original sample period and verified
that all replay sample payloads remain byte-identical. Pilot CFI replay reproduced
the first costing and binding chains; all 35 caller IPs in those examples fall on
the last byte of real call instructions, matching perf's return-address adjustment.

This correction recovers planning callers from existing captures. It does not make
finite snapshots complete. The pilot still has out-of-range reads at deeper frames.
Its [quality.json](pilot/quality.json) records the failed original decoding gate;
that historical result is retained unchanged.

## Audit and provenance

Run the portable checks with Python 3.9+ on a POSIX host:

```sh
python3 /path/to/2026-11-02-planning-cpu/verify.py
```

The audit hashes exports, regenerates the fixture digest independently, parses the
twelve client logs, recomputes planning-ancestry weights from decoded IPs and recorded
original symbol ranges, and checks recorded identities, counters and cleanup.
It uses the frozen parser, which imports the Unix `resource` module.

The export omits original/replay `.data` files, executable bytes, fixture data and
large per-frame classified JSON. Those remain at the [recorded local capture path](manifest.json).
The portable audit cannot independently recheck raw sample preservation, ELF CFI,
raw-to-decoded matching, or actual executable bytes. The local audit and independent
review performed those checks. Export hashes establish integrity of the exported
records, not reproducible compilation. Historical build-provenance limits remain.
Absolute paths in captured commands preserve cache provenance.

All three engines exited zero. A final live check found their PIDs absent and ports
53332–53334 refusing connections. The reused profiler is stopped, with both memory
and swap restored to 512 MiB. Data and original captures are retained.

## Next candidate

Keep the existing estimator and remove discarded work first. `estimate_builder`
computes needed-column masks twice; one traversal can supply both consumers.
Session root-cost calculation also consumes only `estimate(...).total`, while the
existing result extraction traverses width/disabled metadata and clones output
columns. A total-only entry point can retain the full builder, parallel settlement,
InitPlan charging, and session planning charge while omitting that extraction.

That candidate remains unmeasured and unverified. General formatting suppression
is deferred because rendered strings also carry ordering and subplan semantics.
No engine optimization or production setting change is part of this diagnostic.
