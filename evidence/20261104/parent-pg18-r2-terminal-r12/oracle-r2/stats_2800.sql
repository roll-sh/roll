\set ON_ERROR_STOP on
\set VERBOSITY verbose
\pset pager off
\pset format unaligned
\pset tuples_only on
SET client_encoding='UTF8';
SET statement_timeout='60s';
CREATE FUNCTION cs_text(n int) RETURNS text LANGUAGE sql IMMUTABLE AS $$
SELECT left(string_agg(md5(g::text), '' ORDER BY g), n)
FROM generate_series(1, (n+31)/32) g $$;
CREATE TABLE cs(id int PRIMARY KEY, payload text) WITH (autovacuum_enabled=off);
CREATE INDEX cs_i ON cs(id) INCLUDE(payload);
INSERT INTO cs VALUES (1, 'small');
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','initial','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','initial','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
\echo BEGIN_RESULT stats_2800_single
\set ON_ERROR_STOP off
INSERT INTO cs VALUES (2, cs_text(2800));
\echo RESULT stats_2800_single :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','single','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','single','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
\echo BEGIN_RESULT stats_2800_multi
\set ON_ERROR_STOP off
INSERT INTO cs VALUES (2, 'small'), (3, cs_text(2800));
\echo RESULT stats_2800_multi :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','multi','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','multi','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
\echo BEGIN_RESULT stats_2800_small
\set ON_ERROR_STOP off
INSERT INTO cs VALUES (2, 'small');
\echo RESULT stats_2800_small :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','small','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','small','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','before_update','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','before_update','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
\echo BEGIN_RESULT stats_2800_update
\set ON_ERROR_STOP off
UPDATE cs SET payload=cs_text(2800) WHERE id=1;
\echo RESULT stats_2800_update :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','update','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','update','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','before_upsert','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','before_upsert','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
\echo BEGIN_RESULT stats_2800_upsert
\set ON_ERROR_STOP off
INSERT INTO cs VALUES (1, cs_text(2800)) ON CONFLICT(id) DO UPDATE SET payload=EXCLUDED.payload;
\echo RESULT stats_2800_upsert :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','upsert','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','upsert','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','before_speculative','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','before_speculative','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
\echo BEGIN_RESULT stats_2800_speculative
\set ON_ERROR_STOP off
INSERT INTO cs VALUES (5, cs_text(2800)) ON CONFLICT DO NOTHING;
\echo RESULT stats_2800_speculative :SQLSTATE :ERROR
\set ON_ERROR_STOP on
SELECT pg_stat_force_next_flush();
SELECT pg_stat_clear_snapshot();
SELECT json_build_object('tag','stats','boundary','speculative','counters',json_build_array(n_tup_ins,n_tup_upd,n_tup_del,n_live_tup,n_dead_tup,n_ins_since_vacuum)) FROM pg_stat_user_tables WHERE relname='cs';
SELECT json_build_object('tag','state','boundary','speculative','count',count(*),'rows',json_agg(json_build_object('id',id,'payload',payload) ORDER BY id)) FROM cs;
