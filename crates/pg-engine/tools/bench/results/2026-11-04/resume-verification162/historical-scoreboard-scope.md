# Historical comparisons; not current source acceptance

Frozen committed scoreboard: engineed0c85fe, 2026-11-03, quick2 interleaved pairs; 30/77
cells ahead. Host load12–45, only2rounds, no quiet-window or current-source acceptance.
Strongest throughput gaps: range1000.28x, TPC-HSF0.050.42x,bulk100k0.48x,
OLTPload0.58x,insert8 0.63x,update32 0.53x,analytic200k0.72x.
Memory also trails at these loads: OLTPload0.89x,analyticload0.84x,TPC-Hload0.74x,
TPC-Hquery0.75x,bulk0.70x. These ratios are PG/engine for resource use; <1 means worse.
The two-round spread and historical source prohibit general speedup acceptance.

Frozen footprint gates: baselineidleCPU0.01s/5min with absolute0.05slack vs25percent;
100idle30MB with2MBslack vs10percent;fresh3MB with0.5MBslack vs10percent.
Actual condition max(base+slack,base*(1+share)), also must beatPG.
Historical measurements0.03s/5min vsPG0.635s;29.9MBvs211MB;3.03MBvs38.9MB.
Gate methods: cgroupanon+shmem, idle60s scaled5min, fresh3databases running,
engine2048MB cache, dist, container both sides same cores/cpuweight. Keep units/methods.

Historical scaled memory (BENCH querymemoryround3) only survives in committed prose:
raw logs were deleted with scratch (explicitly recorded in BENCH). NOT durable raw proof.
10/50/100/200 active burst sessions: engine212/329/455/712MB vsPG183/318/462/750MB.
200sessionsengine1,723ops/svsPG2,522;CPU2,003vs1,564us/op. This is activeburst,
not a prediction for idletenants or fullplatform.
TPC-HSF1 query: engine1716MBvsPG1690MB,engine19.7–20.0svsPG14s; cache2048MB.
2Mrow window: engine247–249MBvsPG976MB;2.21–2.24svs1.75s. Memory tradeoff
is workloaddependent. Scale10pgbenchpeak322–385MBTPC-BvsPG346MB.
300krow model allocations30.7MB(~93B/row) excludes restofengine and differs fromPG
pages170B/row; do not treat as equal memory metrics or extrapolate pertenantcapacity.

Earlier direct-server idle raw evidence survives at
~/.cache/pg-engine-product-20261102/candidate-vs-baseline-idle/observations/summary.json:
64 connected sessions, 5 settled10s windows: candidateanon+shmem34.1015625MiB,baseline35.14453125MiB,PG129.65234375MiB;
candidateCPU69.51us/s over50.076s. Earlier source, narrow settledwindow only;
no suspension, proxy/controlplane or fleetcapacityclaim. Config/identity files sit alongside.
