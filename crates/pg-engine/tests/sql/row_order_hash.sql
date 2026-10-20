-- golden: raw-order
-- Hash-based nodes put rows out in Postgres's order: hash joins' bucket chains (the most
-- recent insert first; unmatched inner rows bucket by bucket, NULL keys in bucket 0),
-- HashAggregate / HashSetOp groups in simplehash's iteration order over the types' hash
-- functions, sized by the planner's estimates.
create table ro_t (id int, g int, s text, n numeric, f float8, d date, b bool);
insert into ro_t select i, i % 7, 'str' || (i % 13), (i % 5) * 1.5, (i % 3)::float8 / 7,
  date '2024-01-01' + (i % 11), i % 2 = 0 from generate_series(1, 300) i;
analyze ro_t;
select g, count(*) from ro_t group by g;
select s, count(*) from ro_t group by s;
select n, sum(id) from ro_t group by n;
select f, count(*) from ro_t group by f;
select d, count(*) from ro_t group by d;
select b, count(*) from ro_t group by b;
select g, s, count(*) from ro_t group by g, s limit 20;
select distinct s from ro_t;
select distinct g, n from ro_t;
select g from ro_t union select id from ro_t where id < 20;
select s from ro_t where id < 30 union select 'zz' union select s from ro_t where id > 290;
select g from ro_t intersect select id from ro_t where id < 20;
select id from ro_t where id < 20 intersect select g from ro_t;
select g from ro_t except select id from ro_t where id < 3;
select g from ro_t intersect all select id from ro_t where id < 20;
select g from ro_t except all select id from ro_t where id < 3;
select s, g from ro_t except select s, id from ro_t where id < 3;
explain select g from ro_t intersect select id from ro_t where id < 20;
explain select id from ro_t where id < 20 intersect select g from ro_t;
explain select s, g from ro_t except select s, id from ro_t where id < 3;
-- Grouping sets: hashed (MixedAggregate) and sorted (ordered aggregates).
create table ro_h (x int, y text);
insert into ro_h values (1, 'a'), (2, 'b'), (2, 'b'), (3, null), (null, 'c'), (5, 'e');
select x, y, count(*) from ro_h group by rollup (x, y);
select x, y, count(*) from ro_h group by grouping sets ((x), (y));
explain (costs off) select x, y, count(*) from ro_h group by cube (x, y);
select x, y, count(*) from ro_h group by cube (x, y);
explain (costs off) select x, y, rank(x, y) within group (order by x, y) from ro_h group by cube (x, y);
select x, y, rank(x, y) within group (order by x, y) from ro_h group by cube (x, y);
select x, y, rank(x, y) within group (order by x, y) from ro_h group by rollup (y, x);
-- Hash joins.
create table ro_x (a int, t text);
create table ro_y (b int, u text);
create table ro_z (c int, w text);
insert into ro_x select g % 300, 'x' || g from generate_series(1, 3000) g;
insert into ro_y select g % 500, 'y' || g from generate_series(1, 2500) g;
insert into ro_z select g, 'z' || g from generate_series(1, 40) g;
insert into ro_z values (null, 'zn1'), (null, 'zn2');
analyze ro_x; analyze ro_y; analyze ro_z;
explain (costs off) select * from ro_x join ro_y on a = b;
select md5(string_agg(t || u, ',')) from (select * from ro_x join ro_y on a = b) s;
explain (costs off) select * from ro_x full join ro_y on a = b;
select md5(string_agg(coalesce(t, '-') || coalesce(u, '-'), ',')) from (select * from ro_x full join ro_y on a = b) s;
explain (costs off) select * from ro_z right join ro_y on c = b;
select md5(string_agg(coalesce(w, '-') || coalesce(u, '-'), ',')) from (select * from ro_z right join ro_y on c = b) s;
explain (costs off) select * from ro_y where not exists (select 1 from ro_z where c = b);
select md5(string_agg(u, ',')) from (select * from ro_y where not exists (select 1 from ro_z where c = b)) s;
explain (costs off) select * from ro_z where exists (select 1 from ro_y where c = b);
select * from ro_z where exists (select 1 from ro_y where c = b) limit 10;
explain (costs off) select * from ro_z where not exists (select 1 from ro_y where c = b);
select * from ro_z where not exists (select 1 from ro_y where c = b);
create table ro_k (k int, v text);
insert into ro_k values (1, 'b1'), (2, 'b2'), (1, 'b1x'), (3, 'b3'), (1, 'b1y'), (7, 'b7'), (8, 'b8');
analyze ro_k;
select * from ro_t join ro_k on ro_t.id = ro_k.k where ro_t.id < 10;
select * from ro_k k1 full join ro_k k2 on k1.k = k2.k + 1;
