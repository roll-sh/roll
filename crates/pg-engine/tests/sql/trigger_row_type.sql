-- A PL/pgSQL trigger's NEW and OLD have the table's row type.
create table trt (a int, b text);
create function trt_takes(trt) returns int language sql as 'select $1.a';
create function trt_trig() returns trigger language plpgsql as $$
begin
  raise notice 'new: % (%), takes %', new, pg_typeof(new), trt_takes(new);
  new := trt_missing(new);
  return new;
end $$;
create trigger trt_t before insert on trt for each row execute function trt_trig();
insert into trt values (1, 'x');
drop function trt_takes(trt);
drop table trt;
drop function trt_trig();
