-- Partition keys under made operator classes: a btree class's comparison function orders
-- range and list bounds and routes rows; a hash class's support function (made or built-in)
-- hashes the key, for routing and for pruning.
create function puo_rev(int4, int4) returns int language sql immutable
  as $$ select case when $1 = $2 then 0 when $1 > $2 then -1 else 1 end $$;
create operator class puo_rev_ops for type int4 using btree as
  operator 1 > (int4, int4), operator 2 >= (int4, int4), operator 3 = (int4, int4),
  operator 4 <= (int4, int4), operator 5 < (int4, int4), function 1 puo_rev(int4, int4);
create table puo_r (a int4) partition by range (a puo_rev_ops);
create table puo_r1 partition of puo_r for values from (0) to (10);
create table puo_r1 partition of puo_r for values from (10) to (0);
create table puo_r2 partition of puo_r for values from (0) to (-10);
create table puo_r3 partition of puo_r for values from (5) to (-3);
create table puo_r4 partition of puo_r for values from (maxvalue) to (10);
insert into puo_r values (5), (-5), (10), (0);
insert into puo_r values (100);
insert into puo_r values (-11);
select tableoid::regclass, a from puo_r order by 1, 2;
update puo_r set a = -6 where a = 5;
select tableoid::regclass, a from puo_r order by 1, 2;
select relname, pg_get_expr(relpartbound, oid) from pg_class where relname like 'puo_r_' order by 1;
create table puo_l (a int4) partition by list (a puo_rev_ops);
create table puo_l1 partition of puo_l for values in (1, 2);
create table puo_l2 partition of puo_l for values in (2);
insert into puo_l values (2), (3);
select tableoid::regclass, a from puo_l order by 1, 2;
drop table puo_r, puo_l;
drop operator class puo_rev_ops using btree;
drop function puo_rev(int4, int4);
-- Hash partitioning.
create function puo_hash(value int4, seed int8) returns int8 language sql strict immutable
  as $$ select value + seed $$;
create operator class puo_hash_ops for type int4 using hash as
  operator 1 =, function 2 puo_hash(int4, int8);
create operator class puo_hash_b for type int4 using hash as
  operator 1 =, function 2 hashint4extended(int4, int8);
create table puo_h (a int, b int) partition by hash (a puo_hash_ops, b puo_hash_b);
create table puo_h0 partition of puo_h for values with (modulus 4, remainder 0);
create table puo_h1 partition of puo_h for values with (modulus 4, remainder 1);
create table puo_h2 partition of puo_h for values with (modulus 4, remainder 2);
create table puo_h3 partition of puo_h for values with (modulus 4, remainder 3);
insert into puo_h select g, g % 3 from generate_series(1, 12) g;
insert into puo_h values (null, null), (null, 1), (1, null);
select tableoid::regclass, a, b from puo_h order by 2, 3;
explain (costs off) select * from puo_h where a = 5 and b = 2;
explain (costs off) select * from puo_h where a is null and b is null;
explain (costs off) select * from puo_h where a = 1 and b is null;
explain (costs off) select * from puo_h where a = 5;
select * from puo_h where a = 5 and b = 2;
select satisfies_hash_partition('puo_h'::regclass, 4, 0, 5, 2);
drop table puo_h;
drop operator class puo_hash_ops using hash;
drop operator class puo_hash_b using hash;
drop function puo_hash(int4, int8);
