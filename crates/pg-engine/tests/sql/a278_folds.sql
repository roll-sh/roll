-- agent 278: a strict function of a NULL constant is NULL, whatever its volatility
-- (evaluate_function); the members of an equivalence class in deconstruct_jointree's order
-- (a JOIN's ON before the WHERE) give a relation's derived equality its operand order; a
-- semi join's left side's constant fixes the column it equates on its right side, a semi join
-- below it holding the WHERE's; a unique path hashed on an expression counts its groups from
-- the columns it reads; integer literals with a 0b / 0o / 0x prefix past int4 are int8.
create function a278_strict(text) returns text as 'select coalesce($1, ''null'')' language sql volatile strict;
explain select a278_strict(null);
select a278_strict(null) is null;
create table a278i (code int primary key);
create table a278j (a int not null, num int not null, primary key (a, num));
create table a278k (name int primary key, a int);
explain select * from a278k join a278i on a278k.a = a278i.code join a278j on a278i.code = a278j.a where a278k.a = a278j.num;
create table a278x (u int, v int) with (autovacuum_enabled = off);
insert into a278x select g, g from generate_series(0, 999) g;
create index a278x_u on a278x (u);
analyze a278x;
create table a278y (f int) with (autovacuum_enabled = off);
insert into a278y values (0), (1), (2), (3), (4);
analyze a278y;
explain select count(*) from a278x x where x.u in (select f from a278y a where f > 0) and x.u = 0 and x.u in (select f from a278y aa);
create table a278s (f1 int, f2 int, f3 float8) with (autovacuum_enabled = off);
explain select f1 from a278s where (f1, f2) in (select f2, cast(f3 as int4) from a278s where f3 is not null);
select pg_typeof(0b10000000000000000000000000000000), pg_typeof(-0b10000000000000000000000000000000),
  pg_typeof(0o20000000000), pg_typeof(-0x8000000000000000), pg_typeof(0X7FFFFFFF);
explain select 0b10000000000000000000000000000000;
drop function a278_strict(text);
drop table a278i, a278j, a278k, a278x, a278y, a278s;
