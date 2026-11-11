-- PL/pgSQL compile-time checks: names, targets, cursors, labels, options, statements.
create table gxk_t (a int, b text);
create type gxk_ct as (x int, y text);
create function gxk_a1(p int) returns int language plpgsql as $$
<<outer>>
declare x int; r record; c gxk_ct; exit int; "Quoted" int;
begin
  $1 := 5; p := 6; gxk_a1.p := 7; outer.x := 1; x := 2;
  r := row(1); c.x := 3; c := row(1,'a');
  exit := 1; "Quoted" := 2;
  <<inner>> declare y int; begin inner.y := 1; outer.x := y; end;
  return x;
end $$;
select gxk_a1(1);
create function gxk_a2() returns int language plpgsql as $$
declare x constant int := 1; y int;
begin
  select 1 into y, x;
end $$;
create function gxk_a3() returns int language plpgsql as $$
declare x constant int := 1;
begin
  get diagnostics x = row_count;
end $$;
create function gxk_a4() returns int language plpgsql as $$
declare x constant int := 1;
begin
  for x in select 1 loop end loop;
end $$;
create function gxk_a5() returns int language plpgsql as $$
declare x constant int := 1;
begin
  for x in 1..3 loop end loop;
  return x;
end $$;
select gxk_a5();
create function gxk_a6() returns int language plpgsql as $$
declare x int[];
begin
  open x[1] for select 1;
end $$;
create function gxk_a7() returns int language plpgsql as $$
declare r gxk_ct;
begin
  open r for select 1;
end $$;
create function gxk_a8() returns int language plpgsql as $$
<<b>>
begin
  loop
    continue b;
  end loop;
end $$;
create function gxk_a9() returns int language plpgsql as $$
#variable_conflict nonsense
begin
  return 1;
end $$;
create function gxk_b1() returns int language plpgsql as $$
-- leading comment
#variable_conflict use_column
/* block comment */
#print_strict_params on
begin
  return 1;
end $$;
select gxk_b1();
create function gxk_b2(out a int, out b text) returns setof record language plpgsql as $$
begin
  a := 1; b := 'x'; return next; return;
end $$;
select * from gxk_b2();
create function gxk_b3(out a int) returns setof int language plpgsql as $$
begin
  return query select 1;
  return 1;
end $$;
create function gxk_b4() returns int language plpgsql as $$
declare
  a double precision; b timestamp with time zone; c character varying(10); d int[];
  e int array; f numeric(10,2); g gxk_t.a%type[]; h gxk_ct; i public.gxk_ct;
  j text collate pg_catalog."C"; k gxk_t%rowtype; l int[3][4];
  m no scroll cursor for select 1; n scroll cursor (z int) is select z;
begin
  return 1;
end $$;
select gxk_b4();
create function gxk_b5() returns int language plpgsql as $$
begin
  raise using message = 'x';
  raise 'x %', 1 using hint = 'h';
  raise division_by_zero using message = 'x';
  raise sqlstate '22012' using message = 'y';
  raise info 'x' using detail := 'd', errcode = '22012';
  return 1;
end $$;
create function gxk_b6() returns int language plpgsql as $$
declare x int; y int;
begin
  execute 'select $1, $2' into strict x, y using 1, 2;
  execute 'select 1' using 1 into x;
  execute 'select 1' using 1 using 2;
end $$;
create function gxk_b7() returns int language plpgsql as $$
declare a int[]; x int; s int[];
begin
  foreach x in array a loop end loop;
  foreach s slice 1 in array a loop end loop;
  foreach y in array a loop end loop;
end $$;
create function gxk_b8() returns int language plpgsql as $$
declare x int; t text;
begin
  get diagnostics x = row_count, x := pg_routine_oid;
  get current diagnostics t = pg_context;
  get stacked diagnostics t = message_text, t = returned_sqlstate, t = pg_exception_detail,
    t = pg_exception_hint, t = pg_exception_context, t = column_name, t = constraint_name,
    t = pg_datatype_name, t = table_name, t = schema_name;
  get diagnostics t = message_text;
end $$;
create function gxk_b9() returns int language plpgsql as $$
begin
  case 1 when 1, 2 then null; when 3 then null; else null; end case;
  case when true then null; end case;
  if false then null; elsif true then null; elseif true then null; else null; end if;
  for i in reverse 10..1 by 2 loop null; end loop;
  return 1;
end $$;
select gxk_b9();
create trigger gxk_tr after insert on gxk_t for each row execute function suppress_redundant_updates_trigger();
create function gxk_c1() returns trigger language plpgsql as $$
begin
  new.a := 1; tg_op := 'x'; new.b.c := 1;
  return new;
end $$;
create function gxk_c2() returns trigger language plpgsql as $$
begin
  return;
end $$;
create function gxk_c3() returns trigger language plpgsql as $$
begin
  x := 1;
end $$;
create procedure gxk_c4() language plpgsql as $$
begin
  commit and chain; rollback and no chain; commit; call gxk_c4(); do $d$ begin end $d$;
end $$;
create procedure gxk_c5() language plpgsql as $$
begin
  commit and;
end $$;
create function gxk_c6() returns int language plpgsql as $$
begin
  insert into gxk_t values (1,
end $$;
create function gxk_c7() returns int language plpgsql as $$
begin
  select 1
end $$;
create function gxk_c8() returns int language plpgsql as $$
declare x int;
begin
  insert into gxk_t values (1, 'a') returning a into x;
  import foreign schema s from server x into public;
  create function gxk_inner() returns int begin atomic select 1; select 2; end;
  merge into gxk_t using gxk_t s on true when matched then do nothing;
  return x;
end $$;
create function gxk_c9() returns int language plpgsql as $$
declare é int;
begin
  é := 1;
  select 1 frm;
end $$;
create function gxk_d1() returns int language plpgsql as $$
declare x int;
begin
  x := 1 +;
end $$;
create function gxk_d2() returns int language plpgsql as $$
declare x int[];
begin
  x[1 := 2;
end $$;
create function gxk_d3() returns int language plpgsql as $$
declare x int;
begin
  x = 1 into y;
end $$;
create function gxk_d4() returns int language plpgsql as $$
declare r record;
begin
  r.f;
end $$;
create function gxk_d5() returns int language plpgsql as $$
declare x int;
begin
  while x loop
    exit when;
  end loop;
end $$;
create function gxk_d6() returns int language plpgsql as $$
declare x int;
begin
  perform;
end $$;
create function gxk_d7() returns int language plpgsql as $$
declare x alias for y;
begin
end $$;
create function gxk_d8(a int) returns int language plpgsql as $$
<<lbl>>
declare x int;
begin
  declare y alias for lbl.x; z alias for gxk_d8.a; begin return y + z; end;
end $$;
do $$
declare c cursor (a int, b int) for select a, b; r record;
begin
  for r in c(b := 2, a := 1) loop raise notice '%', r; end loop;
  open c(1, b => 2); close c;
end $$;
do $$ begin for i in 1..2 loop raise notice '%', i; end loop $$;
do $$ begin perform 1; end; $$;
do $$ begin perform 1; end; x $$;
do $$ begin end; end $$;
