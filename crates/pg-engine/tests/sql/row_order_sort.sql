-- golden: raw-order
-- Sorts put rows with equal keys where Postgres's quicksort (sort_template.h) does, a
-- LIMIT's bounded heap and an Incremental Sort theirs; windows, DISTINCT ON, merge joins
-- and ordered aggregates read their input in those orders.
create table ro_k (k int, v text);
insert into ro_k values (1, 'b1'), (2, 'b2'), (1, 'b1x'), (3, 'b3'), (1, 'b1y'), (7, 'b7'), (8, 'b8');
select * from ro_k order by k;
select * from ro_k order by k limit 3;
create table ro_t (id int, g int, s text);
insert into ro_t select i, i % 7, 'str' || (i % 13) from generate_series(1, 300) i;
analyze ro_t;
select id, g from ro_t order by g limit 15;
select id, g from ro_t order by g;
select id, s from ro_t order by s, g limit 40;
select id, s from ro_t order by s desc limit 10 offset 5;
select string_agg(id::text, ',') from ro_t where id < 50 group by g;
create table ro_x (a int, t text);
create table ro_y (b int, u text);
create table ro_z (c int, w text);
insert into ro_x select g % 30, 'x' || g from generate_series(1, 300) g;
insert into ro_y select g % 50, 'y' || g from generate_series(1, 250) g;
insert into ro_z select g, 'z' || g from generate_series(1, 40) g;
analyze ro_x; analyze ro_y; analyze ro_z;
set enable_hashjoin = off;
set enable_nestloop = off;
explain (costs off) select * from ro_z full join ro_y on c = b;
select coalesce(w, '-') || coalesce(u, '-') from ro_z full join ro_y on c = b;
select coalesce(w, '-') || coalesce(u, '-') from ro_z right join ro_y on c = b;
select coalesce(w, '-') || coalesce(u, '-') from ro_z left join ro_y on c = b;
select md5(string_agg(t || u, ',')) from (select * from ro_x join ro_y on a = b where a < 20) s;
reset enable_hashjoin;
reset enable_nestloop;
select a, t, row_number() over (partition by a order by a) from ro_x where a < 4;
select a, t, row_number() over (order by a) from ro_x where a < 4;
select a, t, rank() over (partition by a % 2 order by a), row_number() over (order by t) from ro_x where a < 4;
select a, t, sum(a) over () from ro_x where a < 3;
select a, t, lag(t) over (order by a), first_value(t) over (partition by a % 2 order by a desc) from ro_x where a < 4;
select distinct on (a) a, t from ro_x order by a;
select a, array_agg(t) from ro_x where a < 3 group by a;
select array_agg(t order by a) from ro_x where a < 4;
select a % 2, array_agg(t order by a) from ro_x where a < 6 group by a % 2;
select a % 2, string_agg(distinct t, ',') from ro_x where a < 6 group by a % 2;
select percentile_disc(0.5) within group (order by a), mode() within group (order by a % 3) from ro_x;
-- Incremental Sort: batches of at least 32 rows, then per presorted group.
create table ro_i (a int, b int, c text);
insert into ro_i select g % 100, g % 7, 'c' || g from generate_series(1, 10000) g;
create index on ro_i (a);
analyze ro_i;
explain (costs off) select * from ro_i order by a, b limit 50;
select * from ro_i order by a, b limit 50;
explain (costs off) select * from ro_i where a < 3 order by a, b;
select md5(string_agg(c, ',')) from (select * from ro_i where a < 3 order by a, b) s;
select md5(string_agg(c || r, ',')) from (select a, b, c, row_number() over (order by a, b) r from ro_i where a < 3) s;
