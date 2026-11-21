-- agent 279: system views planned from their definitions (the rewriter's expansion): their
-- rows are the definition's over the catalogs, the plan the definition's relations under the
-- query's quals, and a view of a plain SELECT (a user's too) is pulled up into the query's
-- join search; a catalog's unused LEFT JOIN on a unique key is removed.
create table a279t (id int primary key, code text not null unique, n int check (n > 0));
create table a279c (id int primary key, t int references a279t, note text);
create index a279c_note on a279c (lower(note)) where id > 0;
create view a279v as select a279t.code, a279c.note, a279c.t from a279t left join a279c on a279c.t = a279t.id;
select count(*) > 150 from information_schema.table_constraints;
select count(*) from information_schema.table_constraints where table_schema = 'public';
select constraint_name, constraint_type from information_schema.table_constraints where table_schema = 'public' order by 1;
select table_name, column_name from information_schema.constraint_column_usage where table_schema = 'public' order by 1, 2;
select table_name, column_name, ordinal_position from information_schema.key_column_usage where table_schema = 'public' order by 1, 2;
select count(*) > 100 from pg_indexes;
select tablename, indexname from pg_indexes where schemaname = 'public' order by 2;
select viewname from pg_views where schemaname = 'public';
explain select viewname from pg_views where viewname = 'a279v';
explain select rolname from pg_roles where rolname = 'postgres';
explain select gid from pg_prepared_xacts where gid ~ '^a279' order by gid;
explain select locktype, mode from pg_locks where locktype = 'advisory' order by 1;
explain select name from pg_cursors order by 1;
explain select name from pg_prepared_statements order by 1;
explain select * from a279v where code = 'x';
explain select * from a279v v join a279t t on v.t = t.id where t.n = 3;
