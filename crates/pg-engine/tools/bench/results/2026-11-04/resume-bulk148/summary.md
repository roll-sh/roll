| step | PostgreSQL wall / CPU | engine wall / CPU | engine instr (G) | wall ratio | CPU ratio | written MB (PG / engine) |
|---|---|---|---|---|---|---|
| copy_in | 0.87 / 0.78 | 1.59 / 1.47 | 12.034 | 1.83 | 1.90 | 94 / 66 |
| create_index | 0.39 / 0.59 | 0.83 / 1.32 | 9.137 | 2.16 | 2.22 | 16 / 39 |
| create_unique_index | 0.34 / 0.40 | 0.44 / 0.80 | 9.36 | 1.32 | 2.02 | 39 / 48 |
| copy_out | 0.49 / 0.37 | 0.79 / 1.13 | 13.113 | 1.61 | 3.05 | 0 / 12 |
| insert_select | 0.78 / 0.65 | 1.47 / 1.34 | 10.365 | 1.87 | 2.06 | 184 / 66 |
| update_half | 4.16 / 4.24 | 1.97 / 3.23 | 24.368 | 0.47 | 0.76 | 322 / 71 |
| delete_quarter | 0.32 / 0.22 | 0.69 / 1.27 | 7.439 | 2.18 | 5.70 | 30 / 17 |
| vacuum | 0.34 / 0.34 | 0.43 / 0.36 | 3.523 | 1.28 | 1.07 | 14 / 1 |
| analyze | 0.22 / 0.13 | 0.38 / 0.27 | 2.478 | 1.70 | 2.06 | 0 / 0 |
| count_after | 0.13 / 0.12 | 0.23 / 0.35 | 3.082 | 1.77 | 2.82 | 0 / 0 |
| truncate | 0.10 / 0.03 | 0.11 / 0.01 | 0.531 | 1.13 | 0.36 | 0 / 0 |
| drop_table | 0.15 / 0.06 | 0.11 / 0.12 | 4.647 | 0.74 | 2.14 | 0 / 2 |
total CPU: PostgreSQL 7.93 s, engine 11.67 s, ratio 1.47
eng-1	autovac|0|1|f
eng-1	cont	peak_mb 338	disk_before_drop 0	disk_end 0
eng-1	cgroup memory.peak MB 449
pg-1	autovac|0|1|f
pg-1	pg	peak_mb 956	disk_before_drop 617	disk_end 394
eng-2	autovac|0|1|f
eng-2	cont	peak_mb 324	disk_before_drop 0	disk_end 0
eng-2	cgroup memory.peak MB 455
pg-2	autovac|0|1|f
pg-2	pg	peak_mb 956	disk_before_drop 617	disk_end 394
eng-3	autovac|0|1|f
eng-3	cont	peak_mb 328	disk_before_drop 0	disk_end 0
eng-3	cgroup memory.peak MB 457
pg-3	autovac|0|1|f
pg-3	pg	peak_mb 957	disk_before_drop 617	disk_end 394
