-- Error positions count from the start of the query string, also for a statement that
-- isn't its first (`\;` sends several statements as one simple query).
create table msp_t(a int, b text);
\; select nosuch from msp_t;
\; create function msp_f() returns int language sql as $$ select nosuch $$;
\; create function msp_f() returns int return nosuch;
\; create function msp_f(a int, a int) returns int return 1;
\; create table msp_u(a int check (nosuch > 0));
\; create table msp_u(a int, constraint c check (a > 'x'));
\; alter table msp_t add constraint c check (nosuch > 0);
\; alter table msp_t add column c int check (nosuch > 0);
\; create domain msp_d as int check (value > nosuch);
\; create trigger tr before insert on msp_t for each row when (new.nosuch) execute function nosuch();
\; copy (select nosuch) to stdout;
\; declare c cursor for select nosuch;
\; select 1 +;
\;
  select 'é' || nosuch;
create function msp_f() returns int return nosuch;
create table msp_u(a int check (nosuch > 0));
create domain msp_d as int check (value > nosuch);
create trigger tr before insert on msp_t for each row when (new.nosuch) execute function nosuch();
copy (select nosuch) to stdout;
declare c cursor for select nosuch;
alter table msp_t add constraint c check (nosuch > 0);
