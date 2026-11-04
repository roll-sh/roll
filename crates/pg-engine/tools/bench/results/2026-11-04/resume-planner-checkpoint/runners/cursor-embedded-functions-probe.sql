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
-- An IMMUTABLE custom equality is embedded in IsDistinct, not an Expr::Call child.
create function a165_equal_notice(int, int) returns bool immutable language plpgsql as $$
begin raise notice 'custom equality %, %', $1, $2; return $1 = $2; end $$;
create operator public.=== (leftarg = int, rightarg = int, function = a165_equal_notice);
-- NULLIF chooses the type's equality, so use a domain to keep a distinct operator family
-- out of this bounded probe; ArrayOp directly stores the custom operator definition.
explain (verbose, costs off) declare o scroll cursor for select id operator(public.===) any(array[2, 3]) from a165_checked;
begin;
declare o scroll cursor for select id operator(public.===) any(array[2, 3]) from a165_checked;
fetch all from o;
fetch backward 1 from o;
fetch backward all from o;
rollback;
drop operator public.=== (int, int);
drop function a165_equal_notice(int, int);
drop domain a165_noticing_int;
drop function a165_domain_notice(int);
drop table a165_checked;
