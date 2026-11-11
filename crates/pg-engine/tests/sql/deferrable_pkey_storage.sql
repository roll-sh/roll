create table dpk_d (k int primary key deferrable, v text);
insert into dpk_d select g, 'v' || g from generate_series(1, 5) g;
-- immediate (not deferred): statement-level check
insert into dpk_d values (3, 'x');
update dpk_d set k = k + 1;
select * from dpk_d order by k;
update dpk_d set k = k - 1;
select * from dpk_d order by k;
update dpk_d set k = 6 - k;
select * from dpk_d order by k;
update dpk_d set k = 1 where k = 2;
select * from dpk_d order by k;
-- deferred: fixed before commit
begin;
set constraints dpk_d_pkey deferred;
insert into dpk_d values (1, 'dup1'), (2, 'dup2');
select * from dpk_d order by k, v;
select * from dpk_d where k = 1 order by v;
update dpk_d set k = k + 10 where v like 'dup%';
commit;
select * from dpk_d order by k, v;
-- deferred: still broken at commit
begin;
set constraints all deferred;
insert into dpk_d values (5, 'five');
delete from dpk_d where v = 'v5';
insert into dpk_d values (4, 'four');
commit;
select * from dpk_d order by k, v;
-- savepoints
begin;
set constraints all deferred;
savepoint s1;
insert into dpk_d values (1, 'sp');
rollback to s1;
commit;
select count(*) from dpk_d;
-- ON CONFLICT can't use it as arbiter
insert into dpk_d values (1, 'oc') on conflict (k) do nothing;
insert into dpk_d values (1, 'oc') on conflict do nothing;
insert into dpk_d values (1, 'oc') on conflict on constraint dpk_d_pkey do update set v = 'z';
-- foreign keys can't reference it
create table dpk_r (k int references dpk_d);
create table dpk_r (k int references dpk_d (k));
-- functional dependency doesn't come from it
select k, v from dpk_d group by k;
select count(*), sum(k) from dpk_d;
select indisprimary, indimmediate, indisunique from pg_index where indexrelid = 'dpk_d_pkey'::regclass;
select conname, contype, condeferrable, condeferred from pg_constraint where conrelid = 'dpk_d'::regclass order by conname;
-- multi-column, initially deferred
create table dpk_m (a int, b text, c int, primary key (a, b) deferrable initially deferred);
insert into dpk_m values (1, 'x', 1), (1, 'y', 2), (2, 'x', 3);
begin;
insert into dpk_m values (1, 'x', 4);
select * from dpk_m order by a, b, c;
update dpk_m set a = 9 where c = 4;
commit;
select * from dpk_m order by a, b, c;
insert into dpk_m values (2, 'x', 5);
select * from dpk_m order by a, b, c;
-- adding one to a table with rows
create table dpk_a (k int not null, v int);
insert into dpk_a values (1, 1), (2, 2), (2, 3);
alter table dpk_a add primary key (k) deferrable;
delete from dpk_a where v = 3;
alter table dpk_a add primary key (k) deferrable;
begin;
set constraints all deferred;
insert into dpk_a values (1, 10);
update dpk_a set k = 3 where v = 10;
commit;
select * from dpk_a order by k;
-- dropping it keeps the rows
alter table dpk_a drop constraint dpk_a_pkey;
insert into dpk_a values (1, 99);
select * from dpk_a order by k, v;
-- USING INDEX
create table dpk_u (k int not null, v int);
insert into dpk_u values (1, 1), (2, 2);
create unique index dpk_u_idx on dpk_u (k);
alter table dpk_u add constraint dpk_u_pk primary key using index dpk_u_idx deferrable initially deferred;
begin;
insert into dpk_u values (1, 3);
update dpk_u set k = k * 10 where v = 3;
commit;
select * from dpk_u order by k;
-- LIKE copies it
create table dpk_l (like dpk_d including indexes);
insert into dpk_l values (1, 'a'), (2, 'b');
begin;
set constraints all deferred;
insert into dpk_l values (1, 'c');
select * from dpk_l order by k, v;
rollback;
select * from dpk_l order by k, v;
-- partitioned
create table dpk_p (k int, v text, primary key (k) deferrable) partition by range (k);
create table dpk_p1 partition of dpk_p for values from (0) to (100);
create table dpk_p2 partition of dpk_p for values from (100) to (200);
insert into dpk_p values (1, 'a'), (2, 'b'), (150, 'c');
update dpk_p set k = k + 1;
select tableoid::regclass, * from dpk_p order by k;
begin;
set constraints all deferred;
insert into dpk_p values (2, 'dup');
select * from dpk_p order by k, v;
commit;
select * from dpk_p order by k, v;
-- deleting and reinserting within a deferred transaction
begin;
set constraints all deferred;
insert into dpk_d values (1, 'tmp');
delete from dpk_d where k = 1 and v <> 'tmp';
commit;
select * from dpk_d order by k, v;
-- MERGE
merge into dpk_d d using (values (1, 'm1'), (99, 'm99')) s(k, v) on d.k = s.k
  when matched then update set v = s.v
  when not matched then insert values (s.k, s.v);
select * from dpk_d order by k, v;
-- COPY into a deferred key
begin;
set constraints all deferred;
copy dpk_d from stdin;
1	copied
\.
select * from dpk_d where k = 1 order by v;
commit;
select * from dpk_d order by k, v;
-- index lookups
select v from dpk_d where k = 99;
select v from dpk_d where k between 2 and 4 order by k;
drop table dpk_d, dpk_m, dpk_a, dpk_u, dpk_l, dpk_p;
