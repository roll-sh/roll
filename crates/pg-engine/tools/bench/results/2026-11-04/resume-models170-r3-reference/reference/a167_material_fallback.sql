SET enable_hashjoin=off;
SET enable_mergejoin=off;
SET enable_material=on;
SET enable_memoize=off;
SET max_parallel_workers_per_gather=0;
EXPLAIN (ANALYZE,COSTS OFF,TIMING OFF,SUMMARY OFF,BUFFERS OFF) SELECT count(*) FROM a167_o o1, a167_o o2, a167_o o3, a167_o o4, a167_o o5, a167_o o6, a167_o o7, a167_o o8, a167_n n WHERE o1.k<=2 AND o2.k<=1 AND o3.k<=1 AND o4.k<=1 AND o5.k<=1 AND o6.k<=1 AND o7.k<=1 AND o8.k<=1;
