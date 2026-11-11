-- Errors in SQL-standard function bodies (RETURN / BEGIN ATOMIC) point at the token in
-- the statement where it was written, also when the same token appears earlier.
create table sbp_t1(a int, b text);
create table sbp_t2(c int);
CREATE FUNCTION sbp_e1() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a FROM sbp_t1; SELECT a FROM sbp_t2; END;
CREATE FUNCTION sbp_e2() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT 1 + 'x'::int + 'y'; END;
CREATE FUNCTION sbp_e3(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT x; SELECT x FROM sbp_t1 WHERE b = x; END;
CREATE FUNCTION sbp_e4(x int) RETURNS int LANGUAGE sql RETURN x + x + nosuch;
CREATE FUNCTION sbp_e5(x int) RETURNS int LANGUAGE sql
BEGIN ATOMIC
  SELECT x; -- comment; with semicolon
  SELECT x
    FROM sbp_t1 WHERE a = 'x' || x;
END;
CREATE FUNCTION sbp_e6(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN x > 0 THEN 1 END; SELECT x FROM sbp_t1 WHERE sbp_t1.zz = 1; END;
CREATE FUNCTION sbp_e7(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 'end;'; RETURN x + 'q'; END;
select 1; CREATE FUNCTION sbp_e8() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT a FROM sbp_t1; SELECT a FROM sbp_t2; END;
CREATE FUNCTION sbp_e9(x int) RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT x; SELECT x; END;
CREATE FUNCTION sbp_e10(x text) RETURNS text LANGUAGE sql RETURN x;
select proname, prosrc, pg_get_functiondef(oid) from pg_proc where proname in ('sbp_e9','sbp_e10') order by 1;
drop function sbp_e9, sbp_e10;
drop table sbp_t1, sbp_t2;
