-- A simple SQL function inlined with non-constant arguments: its body's expression
-- evaluates them where it uses them (a CASE only the branch it takes).
create table sfi_t (f1 int);
insert into sfi_t values (0), (123456), (-123456), (2147483647), (-2147483647);
create function sfi_bleat(int) returns int as $$
begin
  raise notice 'bleat %', $1;
  return $1;
end$$ language plpgsql;
create function sfi_if(bool, anyelement, anyelement) returns anyelement as $$
select case when $1 then $2 else $3 end $$ language sql;
select f1, sfi_if(f1 > 0, sfi_bleat(f1), sfi_bleat(f1 + 1)) from sfi_t;
select f1, sfi_if(f1 > 0, f1::int8, f1::int8 + 1) from sfi_t;
create function sfi_add(int, int) returns int as 'select $1 + $2' language sql;
select sfi_add(f1, 1) from sfi_t where f1 < 0 order by 1;
select sfi_add(f1, 1) from sfi_t;
create function sfi_coalesce(int, int) returns int as 'select coalesce($1, $2)' language sql;
select sfi_coalesce(nullif(f1, 0), sfi_bleat(-1)) from sfi_t order by 1;
create view sfi_v as select sfi_if(f1 > 0, f1, -f1) as a, sfi_add(f1, 2) as b from sfi_t;
select pg_get_viewdef('sfi_v');
select * from sfi_v where b < 10 order by a;
drop view sfi_v;
drop table sfi_t;
