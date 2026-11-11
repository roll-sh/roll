-- System catalog scans use the catalogs' indexes (regex.sql's prefix cases: a regular
-- expression's fixed prefix bounds the index scan, its exact form an equality).
explain (costs off) select * from pg_proc where proname ~ 'abc';
explain (costs off) select * from pg_proc where proname ~ '^abc';
explain (costs off) select * from pg_proc where proname ~ '^abc$';
explain (costs off) select * from pg_proc where proname ~ '^abcd*e';
explain (costs off) select * from pg_proc where proname ~ '^abc+d';
explain (costs off) select * from pg_proc where proname ~ '^(abc)(def)';
explain (costs off) select * from pg_proc where proname ~ '^(abc)$';
explain (costs off) select * from pg_proc where proname ~ '^(abc)?d';
explain (costs off) select * from pg_proc where proname ~ '^abcd(x|(?=\w\w)q)';
explain (costs off) select * from pg_class where relname = 'pg_proc';
explain (costs off) select * from pg_class where oid = 1259;
