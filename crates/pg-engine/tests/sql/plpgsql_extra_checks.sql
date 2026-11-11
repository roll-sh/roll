-- plpgsql.extra_warnings / extra_errors (shadowed_variables, too_many_rows,
-- strict_multi_assignment), #print_strict_params and plpgsql.print_strict_params.
set plpgsql.extra_warnings = 'shadowed_variables';
create function xc_w1(in1 int) returns table (out1 int) language plpgsql as $$
declare
in1 int;
out1 int;
begin
end $$;
create function xc_w2() returns void language plpgsql as $$
declare
f1 int;
c1 cursor (f1 int) for select 1;
begin
  declare f1 int; begin end;
  for f1 in 1..2 loop end loop;
end $$;
do $$ declare found int; begin end $$;
set plpgsql.extra_errors = 'shadowed_variables';
create function xc_w3(f1 int) returns int language plpgsql as $$
declare f1 int; begin return 1; end $$;
set plpgsql.extra_errors = 'foo';
set plpgsql.extra_errors = 'all, shadowed_variables';
set plpgsql.extra_warnings = 'shadowed_variables, too_many_rows';
show plpgsql.extra_warnings;
set plpgsql.extra_warnings = 'ALL';
show plpgsql.extra_warnings;
reset plpgsql.extra_errors;
reset plpgsql.extra_warnings;
show plpgsql.extra_errors;
create table xc_t (f1 int, f2 text);
insert into xc_t values (1, 'a'), (2, 'b'), (3, 'c');
create type xc_pair as (a int, b int);
set plpgsql.extra_warnings to 'too_many_rows';
do $$
declare x int;
begin
  select v from generate_series(1,2) g(v) into x;
  execute 'select v from generate_series(1,2) g(v)' into x;
  raise notice 'x = %', x;
end $$;
set plpgsql.extra_errors to 'too_many_rows';
do $$
declare x int;
begin
  select v from generate_series(1,2) g(v) into x;
end $$;
reset plpgsql.extra_errors;
reset plpgsql.extra_warnings;
set plpgsql.extra_warnings to 'strict_multi_assignment';
do $$
declare
  x int;
  y int;
  p xc_pair;
begin
  select 1 into x, y;
  select 1,2 into x, y;
  select 1,2,3 into x, y;
  select 1 into x;
  select 1, 2 into x;
  select 1, 2 into p;
  select 1 into p;
  select 1, 2, 3 into p;
  select 1 where false into x, y;
  for x, y in select 1 loop end loop;
end $$;
set plpgsql.extra_errors to 'strict_multi_assignment';
do $$
declare
  x int;
  y int;
begin
  select 1,2 into x, y;
  raise notice 'ok';
  select 1 into x, y;
end $$;
reset plpgsql.extra_errors;
reset plpgsql.extra_warnings;
create function xc_strict() returns void language plpgsql as $$
declare
x record;
p1 int := 1;
p3 text := 'foo';
begin
  select * from xc_t where f1 > p1 or f2 = p3 into strict x;
end $$;
select xc_strict();
set plpgsql.print_strict_params to true;
create or replace function xc_strict() returns void language plpgsql as $$
declare
x record;
p1 int := 1;
p3 text := 'it''s';
begin
  select * from xc_t where f1 > p1 into strict x;
end $$;
select xc_strict();
create or replace function xc_strict() returns void language plpgsql as $$
declare
x record;
p1 int := 7;
p3 text;
begin
  select * from xc_t where f1 > p1 or f2 = p3 into strict x;
end $$;
select xc_strict();
create or replace function xc_strict() returns void language plpgsql as $$
declare
x record;
begin
  execute 'select * from xc_t where f1 > $1 or f2 = $2' using 0, 'foo' into strict x;
end $$;
select xc_strict();
create or replace function xc_strict() returns void language plpgsql as $$
declare
x record;
begin
  execute 'select * from xc_t where f1 = $1' using 9 into strict x;
end $$;
select xc_strict();
create or replace function xc_strict() returns void language plpgsql as $$
declare
x record;
begin
  select * from xc_t into strict x;
end $$;
select xc_strict();
reset plpgsql.print_strict_params;
create or replace function xc_strict() returns void language plpgsql as $$
-- the option wins over the setting
#print_strict_params on
declare
x record;
p1 int := 1;
begin
  select * from xc_t where f1 > p1 into strict x;
end $$;
select xc_strict();
create or replace function xc_strict() returns void language plpgsql as $$
#print_strict_params on
declare
x int;
p1 int := 1;
begin
  update xc_t set f2 = f2 where f1 > p1 returning f1 into x;
end $$;
select xc_strict();
set plpgsql.print_strict_params to on;
create or replace function xc_strict() returns void language plpgsql as $$
#print_strict_params off
declare
x record;
p1 int := 1;
begin
  select * from xc_t where f1 > p1 into strict x;
end $$;
select xc_strict();
reset plpgsql.print_strict_params;
