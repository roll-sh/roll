-- Aggregate-call resolution order, FILTER nesting, DISTINCT ORDER BY coercions, sharing
SELECT gb_nosuch(DISTINCT 1, 2);
SELECT gb_nosuch(*);
SELECT gb_nosuch(DISTINCT 1, 2 ORDER BY 1, 2);
SELECT gb_nosuch(3) WITHIN GROUP (ORDER BY 1);
SELECT lower(DISTINCT 'a');
SELECT max(x) FILTER (WHERE sum(x) > 0) FROM generate_series(1, 3) x;
SELECT (SELECT max(x) FILTER (WHERE sum(y) > 0) FROM generate_series(1, 3) y) FROM generate_series(1, 3) x;
SELECT (SELECT max(x) FILTER (WHERE sum(x) > 0) FROM generate_series(1, 2) y) FROM generate_series(1, 3) x;
CREATE TABLE gb_vc (f1 varchar(10));
INSERT INTO gb_vc VALUES ('b'), ('a'), ('b'), ('c');
SELECT string_agg(DISTINCT f1, ',' ORDER BY f1) FROM gb_vc;
SELECT string_agg(DISTINCT f1::text, ',' ORDER BY f1) FROM gb_vc;
SELECT string_agg(DISTINCT f1, ',' ORDER BY f1::text) FROM gb_vc;
SELECT string_agg(DISTINCT f1, ',' ORDER BY f1 USING ~>~) FROM gb_vc;
SELECT array_agg(f1 ORDER BY f1 USING ~<~ NULLS FIRST) FROM gb_vc;
SELECT booland_statefunc(true, false), boolor_statefunc(true, false), booland_statefunc(NULL, true) IS NULL;
CREATE TYPE gb_rec AS (x numeric);
SELECT row(1.0)::gb_rec *= ANY (ARRAY[row(1.00)::gb_rec, row(1.0)::gb_rec]);
SELECT row(1)::gb_rec = ANY (ARRAY[row(1.00)::gb_rec]);
CREATE AGGREGATE gb_avg (int4) (sfunc = int4_avg_accum, stype = _int8, finalfunc = int8_avg, initcond = '{0,0}');
SELECT gb_avg(x) FROM generate_series(1, 4) x;
SELECT gb_avg(x) FROM generate_series(1, 0) x;
CREATE FUNCTION gb_least_accum(anyelement, VARIADIC anyarray) RETURNS anyelement
  LANGUAGE sql AS 'select least($1, min($2[i])) from generate_subscripts($2,1) g(i)';
CREATE AGGREGATE gb_least_agg(VARIADIC items anyarray) (stype = anyelement, sfunc = gb_least_accum);
SELECT gb_least_agg(a, b) FROM (VALUES (5, 3), (-2, 8)) v(a, b);
SELECT gb_least_agg(VARIADIC ARRAY[a, b]) FROM (VALUES (5, 3), (-2, 8)) v(a, b);
CREATE AGGREGATE gb_pdisc(float8 ORDER BY anyelement) (stype = internal, sfunc = ordered_set_transition,
  finalfunc = percentile_disc_final, finalfunc_extra = true, finalfunc_modify = read_write);
CREATE AGGREGATE gb_rank(VARIADIC "any" ORDER BY VARIADIC "any") (stype = internal,
  sfunc = ordered_set_transition_multi, finalfunc = rank_final, finalfunc_extra = true, hypothetical);
SELECT gb_pdisc(0.5) WITHIN GROUP (ORDER BY x) FROM generate_series(1, 9) x;
SELECT gb_rank(3) WITHIN GROUP (ORDER BY x) FROM (VALUES (1), (1), (2), (4)) v(x);
SELECT array(SELECT percentile_disc(a) WITHIN GROUP (ORDER BY x) FROM (VALUES (0.3), (0.7)) v(a) GROUP BY a)
  FROM generate_series(1, 3) g(x);
SELECT rank('adam'::text COLLATE "C") WITHIN GROUP (ORDER BY x COLLATE "POSIX") FROM (VALUES ('fred')) v(x);
CREATE FUNCTION gb_sum_tr(int8, int4) RETURNS int8 LANGUAGE plpgsql AS
  $$ BEGIN RAISE NOTICE 'gb_sum_tr %', $2; RETURN coalesce($1, 0) + $2; END $$;
CREATE FUNCTION gb_half(int8) RETURNS int8 LANGUAGE sql AS 'select $1 / 2';
CREATE AGGREGATE gb_sum(int4) (stype = int8, sfunc = gb_sum_tr);
CREATE AGGREGATE gb_half_sum(int4) (stype = int8, sfunc = gb_sum_tr, finalfunc = gb_half);
SELECT gb_sum(x), gb_sum(x) FROM (VALUES (1), (3)) v(x);
SELECT gb_sum(x), gb_half_sum(x) FROM (VALUES (1), (2), (4)) v(x);
SELECT gb_sum(DISTINCT x), gb_half_sum(DISTINCT x) FROM (VALUES (1), (3), (1)) v(x);
SELECT gb_sum(DISTINCT x), gb_sum(x) FROM (VALUES (1), (3)) v(x);
DROP AGGREGATE gb_sum(int4);
DROP AGGREGATE gb_half_sum(int4);
DROP FUNCTION gb_sum_tr(int8, int4);
DROP FUNCTION gb_half(int8);
DROP AGGREGATE gb_pdisc(float8 ORDER BY anyelement);
DROP AGGREGATE gb_rank(VARIADIC "any" ORDER BY VARIADIC "any");
DROP AGGREGATE gb_least_agg(VARIADIC anyarray);
DROP FUNCTION gb_least_accum(anyelement, VARIADIC anyarray);
DROP AGGREGATE gb_avg(int4);
DROP TYPE gb_rec;
DROP TABLE gb_vc;
