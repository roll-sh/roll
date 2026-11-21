-- agent 279: catalog columns of type pg_node_tree hold nodeToString's text, as tools read
-- them (pg_constraint.conbin of a table's and a domain's CHECK, pg_index.indexprs / indpred,
-- pg_trigger.tgqual over OLD and NEW, pg_type.typdefaultbin, pg_proc.proargdefaults); a
-- domain's NOT NULL constraint has no conbin; pg_get_expr prints each back.
create domain a279d as int not null default 1 check (value > 0);
create table a279n (id int primary key, qty int check (qty >= 0), note text, amount numeric(12, 2) check (amount >= 0));
create index a279n_lower on a279n (lower(note)) where qty > 0;
create index a279n_two on a279n ((qty + 1), upper(note));
create function a279f() returns trigger language plpgsql as $$ begin return new; end $$;
create trigger a279tr before update on a279n for each row when (new.qty > 0) execute function a279f();
create function a279g(a int, b int default 1, c text default 'x') returns int language sql as $$ select a + b $$;
select conname, contype, conbin from pg_constraint where conrelid = 'a279n'::regclass and contype = 'c' order by 1;
select conname, contype, conbin is null from pg_constraint where contypid = 'a279d'::regtype order by 1;
select conname, conbin from pg_constraint where contypid = 'a279d'::regtype and contype = 'c';
select indexrelid::regclass, indexprs, indpred from pg_index where indrelid = 'a279n'::regclass and (indexprs is not null or indpred is not null) order by 1;
select tgname, tgqual from pg_trigger where tgname = 'a279tr';
select typname, typdefaultbin, typdefault from pg_type where typname = 'a279d';
select proname, proargdefaults from pg_proc where proname = 'a279g';
select conname, pg_get_expr(conbin, conrelid) from pg_constraint where conrelid = 'a279n'::regclass and contype = 'c' order by 1;
select indexrelid::regclass, pg_get_expr(indexprs, indrelid), pg_get_expr(indpred, indrelid, true) from pg_index where indrelid = 'a279n'::regclass and (indexprs is not null or indpred is not null) order by 1;
select pg_get_expr(proargdefaults, 0) from pg_proc where proname = 'a279g';
select pg_get_expr(typdefaultbin, 0) from pg_type where typname = 'a279d';
select pg_get_constraintdef(oid) from pg_constraint where contypid = 'a279d'::regtype and contype = 'c';
