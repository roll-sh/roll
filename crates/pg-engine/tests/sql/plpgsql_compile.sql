-- PL/pgSQL compile-time checks (pl_gram.y / pl_comp.c): errors at CREATE FUNCTION, DO, and a
-- function's first call, with positions or "compilation of ... near line N".
create table gxk_t0 (a int, b text);
create function gxk_f1() returns int language plpgsql as $$
begin
  x := 1;
  return 1;
end $$;
create function gxk_f2() returns int language plpgsql as $$
declare y int;
begin
  select 1 into x;
  return 1;
end $$;
create function gxk_f3() returns int language plpgsql as $$
declare y int; y text;
begin
  return 1;
end $$;
create function gxk_f4() returns int language plpgsql as $$
begin
  continue;
  return 1;
end $$;
create function gxk_f5() returns int language plpgsql as $$
begin
  exit;
end $$;
create function gxk_f6() returns int language plpgsql as $$
begin
  loop exit foo; end loop;
end $$;
create function gxk_f7() returns int language plpgsql as $$
begin
  raise notice 'a % b %', 1;
end $$;
create function gxk_f8() returns int language plpgsql as $$
begin
  raise notice 'a', 1;
end $$;
create function gxk_f9() returns int language plpgsql as $$
begin
  raise notice 'a' using foo = 1;
end $$;
create function gxk_f10() returns setof int language plpgsql as $$
begin
  return 1;
end $$;
create function gxk_f11() returns int language plpgsql as $$
begin
  selec 1;
end $$;
create function gxk_f12() returns int language plpgsql as $$
begin
  select from from gxk_t0;
end $$;
create function gxk_f13() returns int language plpgsql as $$
begin
  if 1 = then null; end if;
end $$;
create function gxk_f14() returns int language plpgsql as $$
declare c cursor (p int) for select p;
begin
  open c;
end $$;
create function gxk_f15() returns int language plpgsql as $$
declare c cursor for select 1;
begin
  open c(1);
end $$;
create function gxk_f16() returns int language plpgsql as $$
declare c cursor (p int, q int) for select p;
begin
  open c(1);
end $$;
create function gxk_f17() returns int language plpgsql as $$
declare x constant int := 1;
begin
  x := 2;
end $$;
create function gxk_f18() returns int language plpgsql as $$
declare x int;
begin
  for x, y in select 1, 2 loop end loop;
end $$;
create function gxk_f19() returns int language plpgsql as $$
begin
  foo.bar := 1;
end $$;
create function gxk_f20() returns int language plpgsql as $$
begin
  return
end $$;
create function gxk_f21() returns int language plpgsql as $$
begin
  perform from;
end $$;
create function gxk_f22() returns int language plpgsql as $$
begin
  get diagnostics x = row_count;
end $$;
create function gxk_f23() returns int language plpgsql as $$
declare x int;
begin
  get diagnostics x = foo;
end $$;
create function gxk_f24() returns int language plpgsql as $$
declare x int;
begin
  get stacked diagnostics x = row_count;
end $$;
create function gxk_f25() returns int language plpgsql as $$
declare x int;
begin
  fetch c into x;
end $$;
create function gxk_f26() returns int language plpgsql as $$
declare x int;
begin
  open x for select 1;
end $$;
create function gxk_f27() returns int language plpgsql as $$
declare x int;
begin
  while x loop
end $$;
create function gxk_f28() returns int language plpgsql as $$
<<lbl>>
declare x int;
begin
  begin continue lbl; end;
end $$;
create function gxk_f29() returns int language plpgsql as $$
declare x int not null;
begin
end $$;
create function gxk_f30() returns int language plpgsql as $$
declare x int;
begin
  for i in reverse select 1 loop end loop;
end $$;
do $$ begin x := 1; end $$;
do $$ declare a int; a int; begin end $$;
do $$ begin raise notice '%'; end $$;
do $$ begin selec 1; end $$;
create function gxk_r1() returns int language plpgsql as $$
begin
  if true then return 1
  end if;
end; $$;
create function gxk_r3() returns setof int language plpgsql as $$
begin
  return next;
end $$;
create function gxk_r4(out x int) returns int language plpgsql as $$
begin
  return next 1;
end $$;
create function gxk_r5(out x int) returns setof int language plpgsql as $$
begin
  return next 1;
end $$;
create function gxk_r6() returns void language plpgsql as $$
begin
  return query select 1;
end $$;
create procedure gxk_p1() language plpgsql as $$
begin
  return 1;
end $$;
create procedure gxk_p2(out x int) language plpgsql as $$
begin
  return 1;
end $$;
create function gxk_dd1() returns int language plpgsql as $$
declare
  a int;
  b a%type;
  c gxk_t0%rowtype;
  d gxk_t0.a%type;
  e nosuch%type;
begin
  return 1;
end $$;
create function gxk_dd2() returns int language plpgsql as $$
declare
  c nosuch%rowtype;
begin
  return 1;
end $$;
create function gxk_dd3() returns int language plpgsql as $$
declare
  c gxk_t0.zz%type;
begin
  return 1;
end $$;
create function gxk_dd4() returns int language plpgsql as $$
declare
  c int collate "C";
begin
  return 1;
end $$;
create function gxk_dd5() returns int language plpgsql as $$
declare
  c text collate "nosuch";
begin
  return 1;
end $$;
create function gxk_dd6() returns int language plpgsql as $$
declare
  c nosuchtype;
begin
  return 1;
end $$;
create function gxk_dd7() returns int language plpgsql as $$
declare
  c int[;
begin
  return 1;
end $$;
create function gxk_dd8() returns int language plpgsql as $$
declare
  c;
begin
  return 1;
end $$;
create function gxk_dd9() returns int language plpgsql as $$
declare
  c anyelement;
begin
  return 1;
end $$;
create function gxk_dd10() returns int language plpgsql as $$
declare
  c alias for nosuch;
begin
  return 1;
end $$;
create function gxk_dd11(int) returns int language plpgsql as $$
declare
  c alias for $1;
  <<lbl>>
begin
  return 1;
end $$;
create function gxk_dd12() returns int language plpgsql as $$
declare
  c int := ;
begin
  return 1;
end $$;
create function gxk_l1() returns int language plpgsql as $$
<<a>>
begin
  return 1;
end b $$;
create function gxk_l2() returns int language plpgsql as $$
begin
  return 1;
end b $$;
create function gxk_l3() returns int language plpgsql as $$
begin
  <<x>> loop exit; end loop y;
  return 1;
end $$;
create function gxk_l4() returns int language plpgsql as $$
begin
  <<x>> loop continue gxk_l4; end loop;
  return 1;
end $$;
create function gxk_l5() returns int language plpgsql as $$
begin
  <<x>> x := 1;
end $$;
create function gxk_cc1() returns int language plpgsql as $$
declare c cursor (p int, q int) for select p;
begin
  open c(p := 1, p := 2);
end $$;
create function gxk_cc2() returns int language plpgsql as $$
declare c cursor (p int, q int) for select p;
begin
  open c(z := 1, p := 2);
end $$;
create function gxk_cc3() returns int language plpgsql as $$
declare c cursor (p int) for select p;
begin
  open c(1, 2);
end $$;
create function gxk_cc4() returns int language plpgsql as $$
declare c cursor (p int) for select p; r record;
begin
  for r in c loop end loop;
end $$;
create function gxk_cc5() returns int language plpgsql as $$
declare c refcursor; r record;
begin
  for r in c loop end loop;
end $$;
create function gxk_cc6() returns int language plpgsql as $$
declare c refcursor;
begin
  open c scroll select 1;
end $$;
create function gxk_cc7() returns int language plpgsql as $$
declare c refcursor; x int;
begin
  fetch all from c into x;
end $$;
create function gxk_cc8() returns int language plpgsql as $$
declare c refcursor; x int;
begin
  fetch c into strict x;
end $$;
create function gxk_cc9() returns int language plpgsql as $$
declare c refcursor; x int;
begin
  move forward 2 c;
end $$;
create function gxk_e1() returns int language plpgsql as $$
begin
  return 1;
exception when nosuch then null;
end $$;
create function gxk_e2() returns int language plpgsql as $$
begin
  return 1;
exception when sqlstate '2201' then null;
end $$;
create function gxk_e3() returns int language plpgsql as $$
begin
  return 1;
exception when others then sqlstate := 'x';
end $$;
create function gxk_x1() returns int language plpgsql as $$
declare x int; y int;
begin
  execute 'select 1' into x into y;
end $$;
create function gxk_x2() returns int language plpgsql as $$
declare x int; y int;
begin
  select 1 into x into y;
end $$;
create function gxk_x3() returns int language plpgsql as $$
declare x int; r record;
begin
  select 1 into x, r;
end $$;
create function gxk_x4() returns int language plpgsql as $$
declare x int; r record;
begin
  select 1 into r, x;
end $$;
create function gxk_x5() returns int language plpgsql as $$
begin
  raise exception;
end $$;
create function gxk_x6() returns int language plpgsql as $$
begin
  raise nosuch_condition;
end $$;
create function gxk_x7() returns int language plpgsql as $$
begin
  raise sqlstate '123';
end $$;
create function gxk_x8() returns int language plpgsql as $$
begin
  raise notice 'x' using message := 1, hint 2;
end $$;
create function gxk_x9() returns int language plpgsql as $$
begin
  assert 1 +;
end $$;
create function gxk_y1() returns int language plpgsql as $$
#option dump
#variable_conflict use_column
#print_strict_params on
begin
  return 1;
end $$;
create function gxk_y2() returns int language plpgsql as $$
#print_strict_params maybe
begin
  return 1;
end $$;
create function gxk_y3() returns int language plpgsql as $$
#foo bar
begin
  return 1;
end $$;
create function gxk_z1() returns int language plpgsql as $$
begin
  perform 'abc;
end $$;
create function gxk_z2() returns int language plpgsql as $$
begin
  /* unterminated
end $$;
create function gxk_z3() returns int language plpgsql as $$
begin
  return 123abc;
end $$;
create function gxk_z4() returns int language plpgsql as $$
begin
  for i in 1..10 loop
    foreach x in array array[1] loop end loop;
  end loop;
end $$;
create function gxk_z5() returns int language plpgsql as $$
declare r record;
begin
  for r, x in select 1 loop end loop;
end $$;
create function gxk_z6() returns int language plpgsql as $$
begin
  for i in 1, 2 .. 3 loop end loop;
end $$;
create function gxk_z7() returns int language plpgsql as $$
begin
  case when 1 then null; end;
end $$;
create function gxk_z8() returns int language plpgsql as $$
begin
  table gxk_t0;
end $$;
create function gxk_z9() returns int language plpgsql as $$
begin
  update gxk_t0 set a = 1 where;
end $$;
set check_function_bodies = off;
create function gxk_g1() returns int language plpgsql as $$
begin
  if 1 then
    raise notice '% %', 1;
  end if;
  return 1;
end $$;
select gxk_g1();
create function gxk_g2() returns int language plpgsql as $$
declare x nosuchtype;
begin
  return 1;
end $$;
select gxk_g2();
create function gxk_g3() returns int language plpgsql as $$
begin
  selec 1;
  return 1;
end $$;
select gxk_g3();
create function gxk_g4() returns int language plpgsql as $$
begin
  x := 1;
  return 1;
end $$;
select gxk_g4();
do $$ begin x := 1; end $$;
do $$ begin selec 1; end $$;
reset check_function_bodies;
do $$ begin perform 1 +; end $$;
-- CREATE FUNCTION's own type checks
create function gxk_h1() returns nosuch as $$ begin end $$ language plpgsql;
create function gxk_h2() returns setof nosuch as $$ begin end $$ language plpgsql;
create function gxk_h3(x nosuch) returns int as $$ begin end $$ language plpgsql;
create function gxk_h4() returns table (x nosuch) as $$ begin end $$ language plpgsql;
create function gxk_h5(x nosuch[]) returns int as $$ begin end $$ language plpgsql;
create function gxk_h6(x public.nosuch) returns int as $$ begin end $$ language plpgsql;
create function gxk_h7(x anyelement) returns anyrange as $$ begin end $$ language plpgsql;
create function gxk_h8(x int) returns anyelement as $$ begin end $$ language plpgsql;
create function gxk_h9(x anycompatible) returns anycompatiblerange as $$ begin end $$ language plpgsql;
create function gxk_h10(x anyrange) returns anymultirange as $$ select null $$ language sql;
create function gxk_h11(x int, out y anyelement) as $$ select null $$ language sql;
create function gxk_h12(x anyelement, out y anyrange) as $$ select null $$ language sql;
create function gxk_h13(x anycompatible, out y anycompatiblemultirange) as $$ select null $$ language sql;
