-- Contrib round 3: tsm_system_rows / tsm_system_time, pg_prewarm, pg_freespacemap,
-- pg_visibility, pgstattuple, pgrowlocks, dict_int, dict_xsyn, passwordcheck, sslinfo,
-- intagg's int_array_enum, tcn's refusals.
DROP ROLE IF EXISTS regress_a71_pw;
CREATE EXTENSION tsm_system_rows;
CREATE EXTENSION tsm_system_time;
CREATE TABLE ts (id int PRIMARY KEY, v text);
INSERT INTO ts SELECT i, repeat('x', 200) FROM generate_series(1, 50) i;
SELECT count(*) FROM ts TABLESAMPLE system_rows (7);
SELECT count(*) FROM ts TABLESAMPLE system_rows (0);
SELECT count(*) FROM ts TABLESAMPLE system_rows (500);
SELECT count(*) FROM ts TABLESAMPLE system_rows (-1);
SELECT count(*) FROM ts TABLESAMPLE system_rows (NULL);
SELECT count(*) FROM ts TABLESAMPLE system_rows (1, 2);
SELECT count(*) FROM ts TABLESAMPLE system_rows ('abc');
SELECT count(*) FROM ts TABLESAMPLE system_rows (3) REPEATABLE (1);
SELECT count(*) FROM ts TABLESAMPLE system_time (0);
SELECT count(*) FROM ts TABLESAMPLE system_time (100000);
SELECT count(*) FROM ts TABLESAMPLE system_time (-5);
SELECT count(*) FROM ts TABLESAMPLE nosuch (5);
EXPLAIN (COSTS OFF) SELECT * FROM ts TABLESAMPLE system_rows (3);
CREATE VIEW tsv AS SELECT * FROM ts TABLESAMPLE system_rows (2);
SELECT pg_get_viewdef('tsv');
DROP EXTENSION tsm_system_rows;
DROP VIEW tsv;

CREATE EXTENSION pg_prewarm;
SELECT pg_prewarm('ts');
SELECT pg_prewarm('ts', 'read', 'main', 0, 0);
SELECT pg_prewarm('ts', 'bogus');
SELECT pg_prewarm('ts', 'buffer', 'nofork');
SELECT pg_prewarm('ts', 'buffer', 'main', -1);
SELECT pg_prewarm('ts', 'buffer', 'main', 0, 1000);
SELECT pg_prewarm(NULL);
SELECT pg_prewarm('ts', NULL);
SELECT pg_prewarm('ts', 'buffer', NULL);
SELECT pg_prewarm('ts', 'buffer', 'vm');
SELECT pg_prewarm('ts_pkey') > 0;
CREATE VIEW pv AS SELECT 1;
SELECT pg_prewarm('pv');
DROP VIEW pv;
SELECT autoprewarm_dump_now() >= 0;

CREATE EXTENSION pg_freespacemap;
SELECT count(*) > 0 FROM pg_freespace('ts');
SELECT pg_freespace('ts', -1);
SELECT pg_freespace('ts', 0) >= 0;
VACUUM ts;
SELECT count(*) FILTER (WHERE avail > 0) > 0 FROM pg_freespace('ts');

CREATE EXTENSION pg_visibility;
SELECT count(*) > 0 FROM pg_visibility_map('ts');
SELECT all_visible FROM pg_visibility_map_summary('ts') s, LATERAL (SELECT s.all_visible > 0 AS all_visible) x;
SELECT * FROM pg_check_frozen('ts');
SELECT * FROM pg_check_visible('ts');
SELECT pg_visibility('ts', -1);
SELECT pg_visibility_map('ts_pkey');
SELECT pg_truncate_visibility_map('ts');
SELECT bool_or(all_visible) FROM pg_visibility_map('ts');

CREATE EXTENSION pgstattuple;
CREATE TABLE st (a int PRIMARY KEY);
SELECT table_len, tuple_count, dead_tuple_count, free_space FROM pgstattuple('st');
SELECT version, tree_level, index_size, root_block_no, leaf_pages FROM pgstatindex('st_pkey');
SELECT pg_relpages('st'), pg_relpages('st_pkey');
SELECT pgstatindex('st');
SELECT pgstatginindex('st_pkey');
SELECT pgstathashindex('st_pkey');
SELECT pgstattuple('nosuch');
SELECT tuple_count FROM pgstattuple('ts');
SELECT table_len > 0, approx_tuple_count FROM pgstattuple_approx('ts');

CREATE EXTENSION pgrowlocks;
SELECT * FROM pgrowlocks('ts');
BEGIN;
SELECT id FROM ts WHERE id = 3 FOR SHARE;
SELECT multi, modes FROM pgrowlocks('ts');
ROLLBACK;
SELECT * FROM pgrowlocks('ts_pkey');

CREATE EXTENSION dict_int;
SELECT ts_lexize('intdict', '511673');
SELECT ts_lexize('intdict', '129');
SELECT ts_lexize('intdict', '40865854');
ALTER TEXT SEARCH DICTIONARY intdict (MAXLEN = 4, REJECTLONG = true);
SELECT ts_lexize('intdict', '12345');
ALTER TEXT SEARCH DICTIONARY intdict (MAXLEN = 0);
ALTER TEXT SEARCH DICTIONARY intdict (BOGUS = 1);
CREATE EXTENSION dict_xsyn;
ALTER TEXT SEARCH DICTIONARY xsyn (RULES = 'xsyn_sample', KEEPORIG = false);
SELECT ts_lexize('xsyn', 'supernova');
ALTER TEXT SEARCH DICTIONARY xsyn (RULES = 'nosuch_file');

SET md5_password_warnings = off;
LOAD 'passwordcheck';
CREATE ROLE regress_a71_pw PASSWORD 'short1';
CREATE ROLE regress_a71_pw PASSWORD 'onlyletterspassword';
CREATE ROLE regress_a71_pw PASSWORD 'xx_regress_a71_pw_1';
CREATE ROLE regress_a71_pw PASSWORD 'good_pass_1';
ALTER ROLE regress_a71_pw PASSWORD 'md5a0b5c4ac7b4a1d6ff0a41dc77e98c5a3';
SHOW passwordcheck.min_password_length;
DROP ROLE regress_a71_pw;

CREATE EXTENSION sslinfo;
SELECT ssl_is_used(), ssl_version(), ssl_cipher();
CREATE EXTENSION intagg;
SELECT int_array_enum('{5,3,9}'::int4[]);
CREATE EXTENSION tcn;
SELECT triggered_change_notification();

DROP TABLE ts, st;
DROP EXTENSION tsm_system_time, pg_prewarm, pg_freespacemap, pg_visibility, pgstattuple, pgrowlocks;
DROP EXTENSION dict_int, dict_xsyn, sslinfo, intagg, tcn;
