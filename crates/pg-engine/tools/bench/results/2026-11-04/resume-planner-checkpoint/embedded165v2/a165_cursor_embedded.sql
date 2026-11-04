-- Column arguments preserve runtime domain/operator calls instead of constant folding.
create table a165_checked (id int primary key);
insert into a165_checked values (1), (2), (3);
create function a165_domain_notice(int) returns bool immutable language plpgsql as $$
begin raise notice 'domain check %', $1; return true; end $$;
create domain a165_noticing_int as int check (a165_domain_notice(value));
explain (verbose, costs off) declare d scroll cursor for select id::a165_noticing_int from a165_checked;
begin;
declare d scroll cursor for select id::a165_noticing_int from a165_checked;
fetch all from d;
fetch backward 1 from d;
fetch backward all from d;
rollback;
-- A STABLE custom equality is embedded in ArrayOp/InList, not an Expr::Call child.
create function a165_equal_notice(int, int) returns bool stable language plpgsql as $$
begin raise notice 'custom equality %, %', $1, $2; return $1 operator(pg_catalog.=) $2; end $$;
create operator public.=== (leftarg = int, rightarg = int, function = a165_equal_notice);
-- Two array elements prevent a one-element scalar simplification.
explain (verbose, costs off) declare o scroll cursor for select id operator(public.===) any(array[2, 3]) from a165_checked;
begin;
declare o scroll cursor for select id operator(public.===) any(array[2, 3]) from a165_checked;
fetch all from o;
fetch backward 1 from o;
fetch backward all from o;
rollback;
-- Explicitly put public before pg_catalog so IN resolves this equality, not int4eq.
create operator public.= (leftarg = int, rightarg = int, function = a165_equal_notice);
begin;
set local search_path = public, pg_catalog;
explain (verbose, costs off) declare i scroll cursor for select id in (2, 3) from a165_checked;
declare i scroll cursor for select id in (2, 3) from a165_checked;
fetch all from i;
fetch backward 1 from i;
fetch backward all from i;
rollback;
drop operator public.= (int, int);
drop operator public.=== (int, int);
drop function a165_equal_notice(int, int);
drop domain a165_noticing_int;
drop function a165_domain_notice(int);
drop table a165_checked;
