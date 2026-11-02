# Rejected Gather proof experiment

The candidate was rejected and its source reverted. The retained baseline executable was restored by atomic replacement; this was not a fresh rebuild. No speedup is retained.

The primary 12-pair test passed (paired Hodges–Lehmann gain 13.03%, exact one-sided p=0.00024414). The five-pair declined-proof control failed: median paired throughput fell 4.91% for range8 and 4.13% for point1, below the required 0.97 ratio. The actual-Gather control passed, with low server utilization and coarse CPU tick resolution; it excludes only a large regression. The predeclared decision therefore rejects the candidate.

76 benchmark invocations including warmups checked 11,168,976 operations, with zero errors. Separately, 1,655 actual-Gather queries checked 6,620,000 rows. CPU/op under saturation is coupled to throughput. This was a shared host, with counterbalanced order and recorded CPU affinity and niceness.

Run `python3 -I verify.py` to verify manifest integrity, source snapshots, runtime identity, recorded cleanup and the statistics from raw results. This reads saved records only and reuses the saved statistical evaluator. It does not independently audit run ordering, settings parity, or the actual-Gather EXPLAIN shape. The independent reviewer checked those separately. Binaries and database copies are omitted; the verifier cannot independently establish historical live state or reproduce the workload. Full candidate workspace, PostgreSQL and regression acceptance gates were skipped after performance rejection. Focused/library/Clippy evidence remains bounded. PostgreSQL superiority, compatibility, recovery and distribution remain open.

Source inspection shows an extra serial costing pass on a declined proof; its share of the observed regression was not profiled or counted. The next design must count and remove that duplicate work. Old primary gains cannot certify a new candidate.
